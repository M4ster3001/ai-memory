//! `ai-memory agent-bus install | status | doctor`: put the agent bus on a
//! host with one command and see whether it works (`docs/agent-bus/`).
//!
//! `install` shows what it will do and asks once: it writes the per-user
//! service that runs `ai-memory message supervise` (systemd on Linux, a
//! LaunchAgent on macOS), adds the Claude Code `Stop` hook that wakes an open
//! session (`message watch --rewake`) to `~/.claude/settings.json`, and starts
//! the service. There is no configuration file to write: agents are
//! discovered from hook sessions, and the programs of the known adapters are
//! trusted on first use by the supervisor. The hook and the service read the
//! server and its token from this installation's own configuration, so no
//! token is written into the hook.
//!
//! `status` and `doctor` read the server's view (`GET /inbox/bus/status`:
//! discovered agents, their newest wake outcome, paused pairs and trusted
//! programs) and the host's (service, hook, programs on the search path).

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_memory_core::message_wake::AdapterKind;
use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::message_watch::refused_credential;
use super::render_shared::{is_claude_rewake_handler, shell_quote};
use super::user_service::{self, ServiceManager, ServiceSpec};
use crate::cli::{AgentBusArgs, AgentBusCommand, AgentBusInstallArgs};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json_bounded, server_unreachable};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REPLY_BYTES: usize = 4 << 20;

/// Run `ai-memory agent-bus`. Returns the process exit code.
///
/// # Errors
/// An unusable host for a per-user service, an unreadable Claude Code
/// settings file, or a failed write.
pub(crate) async fn run(
    config: &Config,
    args: AgentBusArgs,
    config_path: Option<&Path>,
) -> Result<i32> {
    match args.command {
        AgentBusCommand::Install(install) => install_command(config, &install, config_path).await,
        AgentBusCommand::Status(status) => status_command(config, config_path, status.json).await,
        AgentBusCommand::Doctor => doctor_command(config, config_path).await,
    }
}

/// What the host has, for `install`, `status` and `doctor`.
struct Host {
    exe: PathBuf,
    config_path: PathBuf,
    manager: Option<ServiceManager>,
    unit: Option<user_service::ServiceUnit>,
    claude_settings: Option<PathBuf>,
    /// The agents found on this machine, by their `install-hooks --agent`
    /// name, with the hooks file that install writes.
    agents: Vec<(&'static str, PathBuf)>,
}

/// The agents `install` wires, found by their configuration directory:
/// `~/.claude`, `$CODEX_HOME` or `~/.codex`, `~/.gemini`.
fn detected_agents(config: &Config) -> Vec<(&'static str, PathBuf)> {
    let candidates = [
        (
            "claude-code",
            super::install_hooks::claude_settings_path().ok(),
        ),
        (
            "codex",
            super::install_hooks::codex_hooks_path(&config.runtime_env).ok(),
        ),
        (
            "gemini-cli",
            super::install_hooks::gemini_settings_path().ok(),
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(agent, path)| {
            let path = path?;
            path.parent()
                .is_some_and(Path::is_dir)
                .then_some((agent, path))
        })
        .collect()
}

/// The command that writes `agent`'s native hooks: this binary's own
/// `install-hooks --apply`, with this installation's data directory and
/// configuration, so the hooks reach the same server with the same token.
fn install_hooks_argv(
    exe: &Path,
    data_dir: &Path,
    config_path: &Path,
    agent: &str,
    hooks_dir: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        "--data-dir".to_owned(),
        data_dir.to_string_lossy().into_owned(),
        "--config".to_owned(),
        config_path.to_string_lossy().into_owned(),
        "install-hooks".to_owned(),
        "--agent".to_owned(),
        agent.to_owned(),
        "--apply".to_owned(),
    ];
    if let Some(dir) = hooks_dir {
        argv.push("--hooks-dir".to_owned());
        argv.push(dir.to_string_lossy().into_owned());
    }
    argv
}

fn host(config: &Config, config_path: Option<&Path>) -> Result<Host> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("locating the running ai-memory binary")?;
    let config_path =
        config_path.map_or_else(|| config.data_dir.join("config.toml"), Path::to_path_buf);
    let manager =
        ServiceManager::current().filter(|manager| *manager != ServiceManager::WindowsRun);
    let home = super::run::native_home(config);
    let unit = match (manager, &home) {
        (Some(manager), Some(home)) => {
            let spec = ServiceSpec {
                exe: exe.clone(),
                data_dir: config.data_dir.clone(),
                config_path: config_path.clone(),
                home: home.clone(),
                xdg_config_home: config
                    .runtime_env
                    .var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from),
            };
            user_service::render_supervisor(manager, &spec).ok()
        }
        _ => None,
    };
    let claude_settings = super::install_hooks::claude_settings_path()
        .ok()
        .filter(|path| path.parent().is_some_and(Path::is_dir));
    Ok(Host {
        exe,
        config_path,
        manager,
        unit,
        claude_settings,
        agents: detected_agents(config),
    })
}

/// The Claude Code wake hook: this binary, this installation's data
/// directory and configuration, `message watch --rewake`. The server and its
/// token come from that configuration, never from the command line.
fn rewake_handler(host: &Host, config: &Config) -> Value {
    let command = format!(
        "{} --data-dir {} --config {} message watch --rewake --agent claude-code",
        shell_quote(&host.exe.to_string_lossy()),
        shell_quote(&config.data_dir.to_string_lossy()),
        shell_quote(&host.config_path.to_string_lossy()),
    );
    json!({
        "type": "command",
        "command": command,
        "asyncRewake": true,
        "timeout": super::message_rewake::REWAKE_HOOK_TIMEOUT_SECS,
    })
}

/// `settings` with exactly one wake hook, `handler`, in its own `Stop`
/// entry. Other hooks, including other ai-memory hooks, stay as they are.
fn with_rewake_hook(mut settings: Value, handler: Value) -> Result<Value> {
    let root = settings
        .as_object_mut()
        .context("the Claude Code settings file is not a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`hooks` in the Claude Code settings file is not an object")?;
    let stop = hooks
        .entry("Stop")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("`hooks.Stop` in the Claude Code settings file is not a list")?;
    for entry in stop.iter_mut() {
        if let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
            handlers.retain(|handler| !is_claude_rewake_handler(handler));
        }
    }
    stop.retain(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|handlers| !handlers.is_empty())
    });
    stop.push(json!({"matcher": "", "hooks": [handler]}));
    Ok(settings)
}

/// Whether the settings file holds the wake hook.
fn has_rewake_hook(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|settings| settings.pointer("/hooks/Stop").cloned())
        .and_then(|stop| stop.as_array().cloned())
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|handlers| handlers.iter().any(is_claude_rewake_handler))
            })
        })
}

/// Whether a hooks file runs the native `ai-memory hook` for `agent`'s
/// `PostToolUse`, the hook that carries the mid-turn mail notice.
fn has_native_tool_hook(path: &Path, agent: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| {
        text.contains("hook --event post-tool-use") && text.contains(&format!("--agent {agent}"))
    })
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(json!({})),
        Ok(raw) => serde_json::from_str(&raw).with_context(|| {
            format!(
                "{} is not valid JSON; fix it before installing the agent bus",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error)
            .with_context(|| format!("reading the Claude Code settings {}", path.display())),
    }
}

/// The commands that load and start the service.
fn start_commands(manager: ServiceManager, unit: &Path) -> Vec<Vec<String>> {
    match manager {
        ServiceManager::SystemdUser => vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec![
                "systemctl".into(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                user_service::SUPERVISOR_SYSTEMD_UNIT_NAME.into(),
            ],
        ],
        _ => vec![vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{}", current_uid()),
            unit.to_string_lossy().into_owned(),
        ]],
    }
}

fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata("/proc/self")
            .or_else(|_| std::fs::metadata(std::env::temp_dir()))
            .map(|meta| meta.uid())
            .unwrap_or(0)
    }
    #[cfg(not(unix))]
    0
}

/// The server's view of the bus, or why it cannot be read.
async fn server_status(config: &Config) -> Result<Value, String> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let reply: Result<Value> = get_json_bounded(
        &endpoint,
        "/inbox/bus/status",
        &[],
        REQUEST_TIMEOUT,
        MAX_REPLY_BYTES,
    )
    .await;
    reply.map_err(|error| {
        if refused_credential(&error) == Some(403) {
            "the server refused: the agent bus needs the root credential".to_owned()
        } else if refused_credential(&error).is_some() {
            "the server refused the credential".to_owned()
        } else if server_unreachable(&error) {
            format!(
                "no ai-memory server answers at {}",
                super::show::terminal_text(&config.server_url)
            )
        } else {
            format!(
                "the server has no agent bus status ({}); upgrade it",
                super::show::terminal_text(&error.root_cause().to_string())
            )
        }
    })
}

fn agent_line(agent: &Value) -> String {
    let text = |key: &str| super::show::terminal_text(agent[key].as_str().unwrap_or_default());
    let session = if agent["open"].as_bool() == Some(true) {
        "session open"
    } else {
        "session ended"
    };
    let wake = agent
        .get("last_wake")
        .filter(|wake| !wake.is_null())
        .map_or_else(
            || "no wake yet".to_owned(),
            |wake| {
                let event = super::show::terminal_text(wake["event"].as_str().unwrap_or_default());
                let reason = wake["reason"]
                    .as_str()
                    .map(|reason| format!("/{}", super::show::terminal_text(reason)))
                    .unwrap_or_default();
                let at = super::show::terminal_text(wake["at"].as_str().unwrap_or_default());
                format!("last wake {event}{reason} at {at}")
            },
        );
    format!(
        "{} in {}/{}: {session}, {wake}",
        text("agent"),
        text("workspace"),
        text("project")
    )
}

fn pair_line(pair: &Value) -> String {
    let side = |side: &Value| {
        format!(
            "{} in {}/{}",
            super::show::terminal_text(side["agent"].as_str().unwrap_or_default()),
            super::show::terminal_text(side["workspace"].as_str().unwrap_or_default()),
            super::show::terminal_text(side["project"].as_str().unwrap_or_default()),
        )
    };
    format!(
        "{} and {}: paused ({}) at {}",
        side(&pair["a"]),
        side(&pair["b"]),
        super::show::terminal_text(pair["reason"].as_str().unwrap_or_default()),
        super::show::terminal_text(pair["paused_at"].as_str().unwrap_or_default()),
    )
}

async fn install_command(
    config: &Config,
    args: &AgentBusInstallArgs,
    config_path: Option<&Path>,
) -> Result<i32> {
    let host = host(config, config_path)?;
    let (Some(manager), Some(unit)) = (host.manager, host.unit.as_ref()) else {
        anyhow::bail!(
            "the agent bus service runs as a systemd user service or a macOS LaunchAgent; this host has neither"
        );
    };
    if super::message_supervise::temp_roots()
        .iter()
        .any(|root| host.exe.starts_with(root))
    {
        anyhow::bail!(
            "this ai-memory binary is under a temporary directory; copy it to a permanent place such as ~/.local/bin and run that copy"
        );
    }
    let status = server_status(config).await;
    let handler = rewake_handler(&host, config);
    let start = start_commands(manager, &unit.path);

    println!("ai-memory agent-bus install will:");
    println!(
        "  1. write the service {} that runs: {} --data-dir {} --config {} message supervise",
        unit.path.display(),
        host.exe.display(),
        config.data_dir.display(),
        host.config_path.display()
    );
    if host.agents.is_empty() {
        println!("  2. write no agent hooks: no ~/.claude, ~/.codex or ~/.gemini directory");
    } else {
        println!(
            "  2. write the native hooks of each agent found (session discovery, and the mail notice between tool calls):"
        );
        for (agent, path) in &host.agents {
            println!(
                "     {agent}: {} (backup kept), as `{}`",
                path.display(),
                install_hooks_argv(
                    &host.exe,
                    &config.data_dir,
                    &host.config_path,
                    agent,
                    args.hooks_dir.as_deref()
                )[5..]
                    .join(" ")
            );
        }
    }
    if let Some(path) = &host.claude_settings {
        println!(
            "     and the Claude Code wake hook (Stop, asyncRewake) in {}: {}",
            path.display(),
            handler["command"].as_str().unwrap_or_default()
        );
    }
    println!(
        "  3. start the service: {}",
        start
            .iter()
            .map(|command| command.join(" "))
            .collect::<Vec<_>>()
            .join(" && ")
    );
    println!(
        "The service wakes agents discovered from hook sessions. It trusts the codex, claude and \
         gemini programs it finds on first use, records their path and SHA-256, and logs and \
         audits a changed binary. No configuration file is written."
    );
    match &status {
        Ok(status) => {
            let agents = status["agents"].as_array().cloned().unwrap_or_default();
            println!("The server knows {} agent(s):", agents.len());
            for agent in &agents {
                println!("  - {}", agent_line(agent));
            }
        }
        Err(why) => println!("Warning: {why}. The service will keep retrying."),
    }
    if args.dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(0);
    }
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "confirming the agent bus install needs a terminal; run it in one or pass --yes"
            );
        }
        print!("Install? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "s" | "sim"
        ) {
            println!("Nothing was installed.");
            return Ok(1);
        }
    }
    if let Some(parent) = unit.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    ai_memory_wiki::write_atomic(&unit.path, unit.contents.as_bytes())
        .with_context(|| format!("writing {}", unit.path.display()))?;
    println!("Wrote {}", unit.path.display());
    let mut hooks_ok = true;
    for (agent, _) in &host.agents {
        let argv = install_hooks_argv(
            &host.exe,
            &config.data_dir,
            &host.config_path,
            agent,
            args.hooks_dir.as_deref(),
        );
        let ran = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            // The Docker wrapper forces the shell scripts through this
            // variable; the bus needs the native hook command.
            .env_remove("AI_MEMORY_HOOK_PLATFORM")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if ran.is_ok_and(|status| status.success()) {
            println!("Wrote the {agent} hooks");
        } else {
            hooks_ok = false;
            println!(
                "Could not write the {agent} hooks; run `{}` to see why",
                argv[5..].join(" ")
            );
        }
    }
    if let Some(path) = &host.claude_settings {
        let settings = read_settings(path)?;
        if path.exists() {
            let backup = path.with_extension("json.ai-memory-backup");
            std::fs::copy(path, &backup)
                .with_context(|| format!("backing up {}", path.display()))?;
        }
        let updated = with_rewake_hook(settings, handler)?;
        let mut bytes = serde_json::to_vec_pretty(&updated)?;
        bytes.push(b'\n');
        ai_memory_wiki::write_atomic(path, &bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        println!(
            "Added the Claude Code wake hook to {} (backup beside it); open sessions get it at their next start",
            path.display()
        );
    }
    let mut started = true;
    for command in &start {
        let ran = std::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::null())
            .status();
        if !ran.is_ok_and(|status| status.success()) {
            started = false;
            println!("Could not run `{}`; run it yourself.", command.join(" "));
            break;
        }
    }
    if started {
        println!("Started the service. Check it with `ai-memory agent-bus status`.");
    }
    Ok(i32::from(!started || !hooks_ok))
}

/// The service's state, as the service manager says it.
fn service_state(host: &Host) -> String {
    let Some(unit) = &host.unit else {
        return "no per-user service manager".to_owned();
    };
    if !unit.path.exists() {
        return "not installed".to_owned();
    }
    let output = match host.manager {
        Some(ServiceManager::SystemdUser) => std::process::Command::new("systemctl")
            .args([
                "--user",
                "is-active",
                user_service::SUPERVISOR_SYSTEMD_UNIT_NAME,
            ])
            .stdin(std::process::Stdio::null())
            .output(),
        _ => std::process::Command::new("launchctl")
            .args([
                "print".to_owned(),
                format!(
                    "gui/{}/{}",
                    current_uid(),
                    user_service::SUPERVISOR_LAUNCHD_LABEL
                ),
            ])
            .stdin(std::process::Stdio::null())
            .output(),
    };
    match (host.manager, output) {
        (Some(ServiceManager::SystemdUser), Ok(out)) => {
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }
        (_, Ok(out)) if out.status.success() => "loaded".to_owned(),
        (_, Ok(_)) => "not loaded".to_owned(),
        (_, Err(_)) => "unknown".to_owned(),
    }
}

/// The programs the supervisor recorded, from its `approvals.json`.
fn trusted_programs(config: &Config) -> Vec<(String, String, String)> {
    let path = config.data_dir.join("supervisor").join("approvals.json");
    let Some(approvals) = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    else {
        return Vec::new();
    };
    approvals["programs"]
        .as_object()
        .map(|programs| {
            programs
                .iter()
                .map(|(adapter, record)| {
                    (
                        adapter.clone(),
                        record["program"].as_str().unwrap_or_default().to_owned(),
                        record["trusted"].as_str().unwrap_or("terminal").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn status_command(config: &Config, config_path: Option<&Path>, as_json: bool) -> Result<i32> {
    let host = host(config, config_path)?;
    let service = service_state(&host);
    let hook = host.claude_settings.as_deref().map(has_rewake_hook);
    let status = server_status(config).await;
    let programs = trusted_programs(config);
    if as_json {
        let out = json!({
            "service": service,
            "claude_wake_hook": hook,
            "server": status.as_ref().ok(),
            "server_error": status.as_ref().err(),
            "trusted_programs": programs.iter().map(|(adapter, program, trusted)| json!({
                "adapter": adapter, "program": program, "trusted": trusted,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(i32::from(status.is_err()));
    }
    println!("service: {}", super::show::terminal_text(&service));
    println!(
        "Claude Code wake hook: {}",
        match hook {
            Some(true) => "installed",
            Some(false) => "missing",
            None => "no ~/.claude directory",
        }
    );
    for (adapter, program, trusted) in &programs {
        println!(
            "program {adapter}: {} ({trusted})",
            super::show::terminal_text(program)
        );
    }
    match &status {
        Ok(status) => {
            let agents = status["agents"].as_array().cloned().unwrap_or_default();
            println!("agents: {}", agents.len());
            for agent in &agents {
                println!("  {}", agent_line(agent));
            }
            for pair in status["paused_pairs"].as_array().into_iter().flatten() {
                println!("  {}", pair_line(pair));
            }
        }
        Err(why) => println!("server: {why}"),
    }
    Ok(i32::from(status.is_err()))
}

/// What a wake failure means for the person, from its enumerated reason.
fn reason_hint(reason: &str) -> &'static str {
    match reason {
        "auth_failed" => {
            "the harness login refused a headless run; for Codex, sign the CLI in with `codex login` (an API key in the API credits promotion cannot run Codex)"
        }
        "binary_missing" => {
            "the supervisor found no program for this harness; install it or put it on the service's PATH"
        }
        "not_approved" => {
            "the program failed the file checks, or trust_on_first_use is off and it needs `ai-memory message supervise approve --adapter`"
        }
        "loop_suspected" => {
            "the loop guard paused this agent with a sender; prompt either session yourself or resume the pair on /web/agent-bus"
        }
        "session_not_found" => {
            "the harness has no such session, or its directory is gone, world-writable or under /tmp"
        }
        "session_busy" => "the session is open in another process; it is woken by its own hook",
        "adapter_timeout" => "the wake ran past its timeout and was stopped",
        _ => "see the supervisor log: journalctl --user -u ai-memory-supervisor",
    }
}

async fn doctor_command(config: &Config, config_path: Option<&Path>) -> Result<i32> {
    let host = host(config, config_path)?;
    let mut failures = 0;
    let mut check = |ok: bool, what: String, fix: &str| {
        if ok {
            println!("ok    {what}");
        } else {
            failures += 1;
            println!("FAIL  {what}\n      fix: {fix}");
        }
    };
    let status = server_status(config).await;
    check(
        status.is_ok(),
        match &status {
            Ok(_) => "the server answers the agent bus with the root credential".to_owned(),
            Err(why) => why.clone(),
        },
        "start the ai-memory server and give this CLI the root token (config.toml or AI_MEMORY_AUTH_TOKEN)",
    );
    let service = service_state(&host);
    check(
        host.unit.as_ref().is_some_and(|unit| unit.path.exists()),
        format!("service installed ({service})"),
        "run `ai-memory agent-bus install --yes`",
    );
    check(
        matches!(service.as_str(), "active" | "loaded"),
        format!("service running ({service})"),
        "run `systemctl --user enable --now ai-memory-supervisor.service` and read `journalctl --user -u ai-memory-supervisor`",
    );
    if let Some(path) = &host.claude_settings {
        check(
            has_rewake_hook(path),
            format!("Claude Code wake hook in {}", path.display()),
            "run `ai-memory agent-bus install --yes`, then restart open Claude Code sessions",
        );
    }
    if let Some(path) = &host.claude_settings {
        check(
            has_native_tool_hook(path, "claude-code"),
            format!(
                "Claude Code mid-turn mail notice (native PostToolUse hook) in {}",
                path.display()
            ),
            "run `ai-memory agent-bus install --yes` (it writes the native hooks; the Docker hook scripts do not send the notice)",
        );
    }
    if let Ok(path) = super::install_hooks::codex_hooks_path(&config.runtime_env)
        && path.parent().is_some_and(Path::is_dir)
    {
        check(
            has_native_tool_hook(&path, "codex"),
            format!(
                "Codex mid-turn mail notice (native PostToolUse hook) in {}",
                path.display()
            ),
            "run `ai-memory agent-bus install --yes` (it writes the native hooks; the Docker hook scripts do not send the notice)",
        );
    }
    if let Ok(status) = &status {
        let agents = status["agents"].as_array().cloned().unwrap_or_default();
        check(
            !agents.is_empty(),
            format!("hook sessions reported {} agent(s)", agents.len()),
            "run `ai-memory agent-bus install --yes`, then open an agent in any project",
        );
        for agent in &agents {
            let kind = agent["handle_kind"].as_str().unwrap_or_default();
            let program = match kind {
                "claude_session" => Some(AdapterKind::ClaudeResume),
                "codex_thread" => Some(AdapterKind::CodexExecResume),
                "gemini_session" => Some(AdapterKind::GeminiResume),
                _ => None,
            };
            if let Some(adapter) = program {
                let found = super::message_supervise::find_program(&config.runtime_env, adapter);
                check(
                    found.is_some(),
                    format!(
                        "{} for {}",
                        adapter.program_name(),
                        super::show::terminal_text(agent["agent"].as_str().unwrap_or_default())
                    ),
                    "install the harness, or put its directory on the service's PATH",
                );
            }
            let failed = agent["last_wake"]["event"]
                .as_str()
                .is_some_and(|event| matches!(event, "wake_failed" | "agent_unavailable"));
            let reason = agent["last_wake"]["reason"].as_str().unwrap_or_default();
            check(!failed, agent_line(agent), reason_hint(reason));
        }
        for pair in status["paused_pairs"].as_array().into_iter().flatten() {
            check(
                false,
                pair_line(pair),
                "prompt either session yourself, or resume the pair on /web/agent-bus",
            );
        }
    }
    if failures == 0 {
        println!("The agent bus is ready.");
    }
    Ok(i32::from(failures > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_for(dir: &Path) -> (Host, Config) {
        let config = Config::default();
        let host = Host {
            exe: PathBuf::from("/opt/ai memory/ai-memory"),
            config_path: dir.join("config.toml"),
            manager: None,
            unit: None,
            claude_settings: Some(dir.join("settings.json")),
            agents: Vec::new(),
        };
        (host, config)
    }

    #[test]
    fn the_wake_hook_carries_no_token_and_replaces_only_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let (host, config) = host_for(tmp.path());
        let handler = rewake_handler(&host, &config);
        let command = handler["command"].as_str().unwrap();
        assert!(
            command.starts_with("'/opt/ai memory/ai-memory' --data-dir "),
            "{command}"
        );
        assert!(command.ends_with(" message watch --rewake --agent claude-code"));
        assert!(!command.contains("TOKEN") && !command.contains("--auth-token"));
        assert_eq!(handler["asyncRewake"], true);

        let existing = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"matcher": "", "hooks": [
                    {"type": "command", "command": "/x/stop.sh"},
                    {"type": "command", "asyncRewake": true,
                     "command": "old message watch --rewake --agent claude-code"}
                ]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/x/start.sh"}]}]
            }
        });
        let once = with_rewake_hook(existing, handler.clone()).unwrap();
        let twice = with_rewake_hook(once.clone(), handler).unwrap();
        assert_eq!(once, twice, "installing again changes nothing");
        assert_eq!(once["model"], "opus");
        let stop = once["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0]["hooks"].as_array().unwrap().len(),
            1,
            "the old hook went"
        );
        assert_eq!(stop[0]["hooks"][0]["command"], "/x/stop.sh");
        assert!(is_claude_rewake_handler(&stop[1]["hooks"][0]));
        assert_eq!(
            once["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "/x/start.sh"
        );

        let path = tmp.path().join("settings.json");
        std::fs::write(&path, serde_json::to_vec(&once).unwrap()).unwrap();
        assert!(has_rewake_hook(&path));
        std::fs::write(&path, "{}").unwrap();
        assert!(!has_rewake_hook(&path));
        assert!(with_rewake_hook(json!([1]), json!({})).is_err());
    }

    #[test]
    fn the_mid_turn_notice_needs_the_native_tool_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"/x/post-tool-use.sh"}]}]}}"#,
        )
        .unwrap();
        assert!(
            !has_native_tool_hook(&path, "codex"),
            "a script hook sends no notice"
        );
        std::fs::write(
            &path,
            r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"'/b/ai-memory' hook --event post-tool-use --agent codex --server-url http://x"}]}]}}"#,
        )
        .unwrap();
        assert!(has_native_tool_hook(&path, "codex"));
        assert!(!has_native_tool_hook(&path, "claude-code"));
        assert!(!has_native_tool_hook(
            &tmp.path().join("missing.json"),
            "codex"
        ));
    }

    #[test]
    fn install_writes_each_agent_through_its_own_install_hooks() {
        let argv = install_hooks_argv(
            Path::new("/b/ai-memory"),
            Path::new("/d"),
            Path::new("/d/config.toml"),
            "codex",
            None,
        );
        assert_eq!(
            argv,
            [
                "/b/ai-memory",
                "--data-dir",
                "/d",
                "--config",
                "/d/config.toml",
                "install-hooks",
                "--agent",
                "codex",
                "--apply"
            ]
        );
    }

    #[test]
    fn every_wake_failure_reason_has_a_hint() {
        for reason in [
            "auth_failed",
            "binary_missing",
            "not_approved",
            "loop_suspected",
            "session_not_found",
            "session_busy",
            "adapter_timeout",
        ] {
            assert!(
                !reason_hint(reason).starts_with("see the supervisor log"),
                "{reason}"
            );
        }
    }
}
