//! `ai-memory message supervise`: wake approved agents when mail arrives.
//!
//! The supervisor is a long-running thin client on the operator's host, the
//! only ai-memory process that starts another program (owner decision Q1).
//! For each `[[messaging.supervisor.agents]]` entry it long-polls
//! `GET /inbox/watch`, asks the server which new messages pass the entry's
//! sender gate (`POST /inbox/wake-candidates`), and runs the entry's adapter
//! once for all of them. What happened goes back to the server as enumerated
//! values (`POST /inbox/wake-report`), and the server updates the delivery
//! state and sends the sender its receipt.
//!
//! What it runs is bounded on every side:
//!
//! - Adapters are a closed enum ([`WakeAdapter`]) with validated fields; the
//!   argv is built without a shell, and the only byte a sender influences is
//!   the canonical message id in the fixed wake text.
//! - An adapter runs only after an operator approved it on a terminal. The
//!   approval is a digest of the entry, the program's canonical path and the
//!   SHA-256 of its content, so a changed binary or entry needs a new one.
//! - The program must be a regular, executable file outside the temporary
//!   directories, owned by the operator or root, not group- or
//!   world-writable, in a directory that is not world-writable.
//! - The child gets a fixed working directory (the state directory), an
//!   environment cleared down to an allowlist, no stdin, a timeout that kills
//!   its process group, and 4 KiB of captured output that only the adapter's
//!   classifier reads and that is then dropped.
//! - Budgets (per agent per hour and per day, global per hour), a backoff
//!   after failures and a quota circuit come from [`WakeLedger`], which is
//!   saved after every change so a restart cannot reset them. At most
//!   `max_concurrent_wakes` adapters run at once, and an agent never has two.
//!
//! Log lines go to stderr as a fixed English protocol for the service
//! manager's journal: entry names, adapter ids, message ids, counts and
//! enumerated outcomes. Never argv, conversation ids, output or sender data.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_memory_core::message_wake::{
    AdapterKind, AdapterRun, ClaudeSessionPresence, WakeAdapter, WakeGate, WakeHandleKind,
    WakeLedger, WakeOutcome, adapter_for_session, claude_session_presence, wake_text,
};
use ai_memory_core::{AgentKind, DeliveryReason, DeliveryState, MessageId};
use anyhow::{Context as _, Result};
use fs2::FileExt as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

use super::message_watch::{
    EXIT_ALREADY_RUNNING, EXIT_AUTH_REFUSED, canonical_message_id, clock, refused_credential,
    transient, valid_cursor,
};
use super::user_service::{self, ServiceManager, ServiceSpec};
use crate::cli::{MessageSuperviseArgs, SuperviseCommand};
use crate::config::{Config, RuntimeEnv, SupervisedAgent, WakeFrom};
use crate::http_client::{
    ServerEndpoint, ServerResponseError, get_json_bounded, post_json_bounded,
};

const WATCH_WAIT: Duration = Duration::from_millis(ai_memory_hooks::MAX_INBOX_WATCH_WAIT_MS);
const REPLY_GRACE: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REPLY_BYTES: usize = 1 << 20;
/// Output kept from each of the adapter's stdout and stderr.
const OUTPUT_KEEP_BYTES: usize = 4_096;
/// How long to keep reading output after the adapter exited: a daemon it left
/// behind may hold the pipe open.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// Largest program the approval hashes.
const MAX_PROGRAM_BYTES: u64 = 1 << 30;
/// Directories any local user can write to; a program under them is refused.
const TEMP_ROOTS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];
/// Searched after the program's own directories.
const SYSTEM_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];
/// The only variables an adapter inherits, besides `PATH` and the harness's
/// own home variable. No ai-memory bearer, no provider key.
const PASSTHROUGH_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];
/// Stand-in message id in the argv shown for approval.
const EXAMPLE_ID: &str = "00000000-0000-0000-0000-000000000000";
const APPROVAL_SCHEMA: &str = "ai-memory.supervisor-approval/v1";
/// Schema of an approval that covers an adapter's program for every
/// discovered agent.
const PROGRAM_APPROVAL_SCHEMA: &str = "ai-memory.supervisor-program-approval/v1";
/// Stand-in session id in the argv shown for a program approval.
const EXAMPLE_HANDLE: &str = "00000000-0000-4000-8000-000000000000";

/// Run `ai-memory message supervise` and its subcommands. Returns the
/// process exit code.
///
/// # Errors
/// No supervised agent configured, an unusable state directory, a server
/// answer that is not transient, or a failed approval.
pub(crate) async fn run(
    config: &Config,
    args: MessageSuperviseArgs,
    config_path: Option<&Path>,
) -> Result<i32> {
    let state_dir = args
        .state_dir
        .clone()
        .unwrap_or_else(|| config.data_dir.join("supervisor"));
    match args.command {
        Some(SuperviseCommand::Approve(approve)) => match (&approve.name, &approve.adapter) {
            (Some(name), None) => approve_command(config, &state_dir, name),
            (None, Some(adapter)) => {
                approve_program_command(config, &state_dir, adapter, approve.program.as_deref())
            }
            _ => anyhow::bail!("name one supervised agent, or one adapter with --adapter"),
        },
        Some(SuperviseCommand::Unit) => unit_command(config, config_path),
        None => supervise(config, &state_dir, args.once).await,
    }
}

/// Where a supervised agent came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A `[[messaging.supervisor.agents]]` entry, approved by its name.
    Configured,
    /// A wake handle hook ingest recorded (`GET /inbox/bus`). Its program
    /// and approval are the ones recorded for its adapter kind.
    Discovered,
}

/// One supervised agent, validated.
#[derive(Debug, Clone)]
struct Entry {
    /// Ledger and log key. A configured entry's `name` (`[a-z0-9-]`), or
    /// `workspace/project/agent` for a discovered one, so the two never
    /// collide.
    name: String,
    origin: Origin,
    agent: AgentKind,
    workspace: String,
    project: String,
    include_shared: bool,
    wake_from: Vec<WakeFrom>,
    /// Mail from a `wake_from` project with no recorded sender session
    /// (`unverified`) wakes too.
    wake_unverified: bool,
    adapter: WakeAdapter,
    program: PathBuf,
    timeout: Duration,
    /// The session a discovered entry reaches, as the bus reported it.
    session: Option<DiscoveredSession>,
}

/// What the bus reported about a discovered agent's newest session.
#[derive(Debug, Clone)]
struct DiscoveredSession {
    kind: WakeHandleKind,
    handle: String,
    /// The directory the session ran in, for an adapter that resumes it
    /// there. Checked again before every run.
    cwd: Option<PathBuf>,
}

impl Entry {
    fn from_config(raw: &SupervisedAgent) -> Result<Self> {
        Ok(Self {
            name: raw.name.clone(),
            origin: Origin::Configured,
            agent: AgentKind::from_wire(&raw.agent),
            workspace: raw.workspace.trim().to_owned(),
            project: raw.project.trim().to_owned(),
            include_shared: raw.include_shared,
            wake_from: raw.wake_from.clone(),
            wake_unverified: raw.wake_unverified,
            adapter: raw.adapter.adapter()?,
            program: raw.adapter.program().to_path_buf(),
            timeout: Duration::from_secs(u64::from(raw.timeout_secs)),
            session: None,
        })
    }

    /// The entry for an agent the bus reported, or `None` when this build
    /// has no adapter for its handle kind in the session's state (an open
    /// Claude Code session is woken by its own `Stop` hook) or the handle is
    /// not valid for it.
    fn discovered(target: &BusAgent) -> Option<Self> {
        let agent = AgentKind::from_wire(&target.agent);
        if agent.as_str() != target.agent {
            return None;
        }
        let kind = WakeHandleKind::parse(&target.handle.kind)?;
        if WakeHandleKind::for_agent(agent) != Some(kind) {
            return None;
        }
        let adapter_kind = adapter_for_session(kind, target.handle.open)?;
        let adapter = adapter_kind.for_handle(kind, &target.handle.id)?;
        Some(Self {
            name: format!("{}/{}/{}", target.workspace, target.project, agent.as_str()),
            origin: Origin::Discovered,
            agent,
            workspace: target.workspace.clone(),
            project: target.project.clone(),
            include_shared: false,
            wake_from: Vec::new(),
            wake_unverified: false,
            adapter,
            program: PathBuf::new(),
            timeout: Duration::from_secs(u64::from(adapter_kind.timeout_secs())),
            session: Some(DiscoveredSession {
                kind,
                handle: target.handle.id.clone(),
                cwd: target.handle.cwd.as_deref().map(PathBuf::from),
            }),
        })
    }

    /// The same agent reached through `kind` instead (a fallback adapter).
    fn with_adapter(&self, kind: AdapterKind) -> Option<Self> {
        let session = self.session.as_ref()?;
        let adapter = kind.for_handle(session.kind, &session.handle)?;
        Some(Self {
            adapter,
            timeout: Duration::from_secs(u64::from(kind.timeout_secs())),
            ..self.clone()
        })
    }
}

fn entries(config: &Config) -> Result<Vec<Entry>> {
    let entries = config
        .messaging
        .supervisor
        .agents
        .iter()
        .map(Entry::from_config)
        .collect::<Result<Vec<_>>>()?;
    if entries.is_empty() && !config.messaging.supervisor.discover {
        anyhow::bail!(
            "no supervised agent is configured; add a [[messaging.supervisor.agents]] entry to config.toml"
        );
    }
    Ok(entries)
}

/// A program that passed the safety checks, with what the approval pins.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckedProgram {
    /// Canonical path; the adapter is started from it, never through `PATH`.
    path: PathBuf,
    /// Directory of the configured path (a symlink's own directory), put
    /// first on the child's `PATH` so an interpreter installed beside it is
    /// found.
    configured_dir: PathBuf,
    /// SHA-256 of the program's content, hex.
    sha256: String,
}

/// Why a program cannot run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgramProblem {
    /// It does not exist or cannot be read.
    Missing,
    /// It exists but fails a safety check.
    Unsafe(&'static str),
}

impl ProgramProblem {
    const fn outcome(self) -> WakeOutcome {
        match self {
            Self::Missing => WakeOutcome::Unavailable(DeliveryReason::BinaryMissing),
            Self::Unsafe(_) => WakeOutcome::Failed(DeliveryReason::NotApproved),
        }
    }

    const fn describe(self) -> &'static str {
        match self {
            Self::Missing => "the program does not exist or cannot be read",
            Self::Unsafe(why) => why,
        }
    }
}

/// Check the program at `configured` and hash it. `owner` is the operator's
/// uid (unix); `temp_roots` are canonical directories it must not be under.
fn check_program(
    configured: &Path,
    owner: Option<u32>,
    temp_roots: &[PathBuf],
) -> Result<CheckedProgram, ProgramProblem> {
    if !configured.is_absolute() {
        return Err(ProgramProblem::Unsafe("the program path is not absolute"));
    }
    let path = std::fs::canonicalize(configured).map_err(|_| ProgramProblem::Missing)?;
    let meta = std::fs::metadata(&path).map_err(|_| ProgramProblem::Missing)?;
    if !meta.is_file() {
        return Err(ProgramProblem::Unsafe("the program is not a regular file"));
    }
    if meta.len() > MAX_PROGRAM_BYTES {
        return Err(ProgramProblem::Unsafe("the program is larger than 1 GiB"));
    }
    if temp_roots.iter().any(|root| path.starts_with(root)) {
        return Err(ProgramProblem::Unsafe(
            "the program is under a temporary directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let mode = meta.permissions().mode();
        if mode & 0o111 == 0 {
            return Err(ProgramProblem::Unsafe("the program is not executable"));
        }
        if mode & 0o022 != 0 {
            return Err(ProgramProblem::Unsafe(
                "the program is group- or world-writable",
            ));
        }
        if owner.is_some_and(|uid| meta.uid() != uid && meta.uid() != 0) {
            return Err(ProgramProblem::Unsafe(
                "the program is owned by another user",
            ));
        }
        let parent = path.parent().map(std::fs::metadata);
        if parent.is_some_and(|dir| dir.is_ok_and(|dir| dir.permissions().mode() & 0o002 != 0)) {
            return Err(ProgramProblem::Unsafe(
                "the program is in a world-writable directory",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = owner;
    let sha256 = hash_file(&path).map_err(|_| ProgramProblem::Missing)?;
    Ok(CheckedProgram {
        configured_dir: configured
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
        path,
        sha256,
    })
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The canonical form of everything an approval covers: what runs (adapter,
/// argv shape, program path and content, working directory, timeout) and
/// for whom (agent, inbox, shared mail, the projects that may wake it and
/// whether their unverified mail does). Budgets are left out: changing them
/// changes how often, not what.
fn approval_digest(entry: &Entry, program: &CheckedProgram, cwd: &Path) -> String {
    let mut wake_from: Vec<String> = entry
        .wake_from
        .iter()
        .map(|scope| format!("{}\u{0}{}", scope.workspace.trim(), scope.project.trim()))
        .collect();
    wake_from.sort();
    let argv = entry.adapter.args("{wake}").join("\u{0}");
    let mut parts = vec![
        APPROVAL_SCHEMA.to_owned(),
        entry.name.clone(),
        entry.agent.as_str().to_owned(),
        entry.workspace.clone(),
        entry.project.clone(),
        entry.include_shared.to_string(),
        wake_from.join("\u{1}"),
        entry.adapter.id().to_owned(),
        argv,
        program.path.to_string_lossy().into_owned(),
        program.sha256.clone(),
        cwd.to_string_lossy().into_owned(),
        entry.timeout.as_secs().to_string(),
    ];
    // Appended only when on, so an entry without the option keeps the digest
    // it was approved under; turning it on or off voids the approval.
    if entry.wake_unverified {
        parts.push("wake_unverified".to_owned());
    }
    let mut hasher = Sha256::new();
    for part in &parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hex(&hasher.finalize())
}

/// The canonical form of what a program approval covers for every
/// discovered agent of one adapter kind: the argv shape (the session handle
/// and the wake text are placeholders), the program path and content, the
/// working directory and the timeout. Who may wake a discovered agent is not
/// part of it: the rule is fixed (mail from its own project, `wake_from` is
/// empty), not something the operator configures.
fn program_digest(kind: AdapterKind, program: &CheckedProgram, cwd: &Path) -> String {
    let parts = [
        PROGRAM_APPROVAL_SCHEMA.to_owned(),
        kind.id().to_owned(),
        kind.argv_shape().join("\u{0}"),
        program.path.to_string_lossy().into_owned(),
        program.sha256.clone(),
        cwd.to_string_lossy().into_owned(),
        kind.timeout_secs().to_string(),
    ];
    let mut hasher = Sha256::new();
    for part in &parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hex(&hasher.finalize())
}

/// `approvals.json` in the state directory.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Approvals {
    #[serde(default)]
    entries: BTreeMap<String, Approval>,
    /// Program approvals for discovered agents, by adapter id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    programs: BTreeMap<String, Approval>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Approval {
    digest: String,
    program: PathBuf,
    sha256: String,
    approved_at: String,
    /// How a program record was made: `terminal` for an explicit approval,
    /// `first_use` when the supervisor trusted it on first use, `changed`
    /// when it trusted a new binary at the recorded path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trusted: Option<String>,
    /// The SHA-256 the record had before a `changed` trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_sha256: Option<String>,
}

impl Approvals {
    fn approves(&self, name: &str, digest: &str) -> bool {
        self.entries
            .get(name)
            .is_some_and(|approval| approval.digest == digest)
    }

    fn approves_program(&self, kind: AdapterKind, digest: &str) -> bool {
        self.programs
            .get(kind.id())
            .is_some_and(|approval| approval.digest == digest)
    }

    /// The program an entry runs: its configured one, or the one recorded
    /// for its adapter kind when it was discovered.
    fn program_for(&self, entry: &Entry) -> Option<PathBuf> {
        match entry.origin {
            Origin::Configured => Some(entry.program.clone()),
            Origin::Discovered => self
                .programs
                .get(entry.adapter.id())
                .map(|approval| approval.program.clone()),
        }
    }

    fn approves_entry(&self, entry: &Entry, program: &CheckedProgram, cwd: &Path) -> bool {
        match entry.origin {
            Origin::Configured => self.approves(&entry.name, &approval_digest(entry, program, cwd)),
            Origin::Discovered => {
                let kind = entry.adapter.kind();
                self.approves_program(kind, &program_digest(kind, program, cwd))
            }
        }
    }
}

fn load_json<T: DeserializeOwned + Default>(config: &Config, path: &Path) -> T {
    match std::fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|error| {
            eprintln!(
                "{}",
                crate::localization::diagnostic(
                    &config.i18n,
                    format!(
                        "message supervise: could not read {} ({error}); starting from empty",
                        path.display()
                    )
                )
            );
            T::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(error) => {
            eprintln!(
                "{}",
                crate::localization::diagnostic(
                    &config.i18n,
                    format!(
                        "message supervise: could not read {} ({error}); starting from empty",
                        path.display()
                    )
                )
            );
            T::default()
        }
    }
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    ai_memory_wiki::write_atomic(path, &bytes)?;
    Ok(())
}

/// Create the state directory, private to the operator, and return the
/// operator's uid (the directory's owner) on unix.
fn prepare_state_dir(state_dir: &Path) -> Result<Option<u32>> {
    std::fs::create_dir_all(state_dir).with_context(|| {
        format!(
            "creating the supervisor state directory {}",
            state_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "creating the supervisor state directory {}",
                    state_dir.display()
                )
            },
        )?;
        let meta = std::fs::metadata(state_dir).with_context(|| {
            format!(
                "creating the supervisor state directory {}",
                state_dir.display()
            )
        })?;
        Ok(Some(meta.uid()))
    }
    #[cfg(not(unix))]
    Ok(None)
}

pub(super) fn temp_roots() -> Vec<PathBuf> {
    TEMP_ROOTS
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}

/// The child's environment: the allowlisted variables the supervisor has,
/// the harness's own home variable, and a `PATH` of the program's
/// directories and the system ones.
fn wake_env(
    adapter: &WakeAdapter,
    program: &CheckedProgram,
    env: &RuntimeEnv,
) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = PASSTHROUGH_ENV
        .iter()
        .chain(adapter.harness_env())
        .filter_map(|key| env.var_os(key).map(|value| (OsString::from(key), value)))
        .collect();
    let mut dirs: Vec<PathBuf> = vec![program.configured_dir.clone()];
    if let Some(dir) = program.path.parent()
        && !dirs.iter().any(|known| known == dir)
    {
        dirs.push(dir.to_path_buf());
    }
    dirs.extend(SYSTEM_PATH.iter().map(PathBuf::from));
    if let Ok(path) = std::env::join_paths(dirs) {
        out.push((OsString::from("PATH"), path));
    }
    out
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0_u8; 4_096];
    // Read to the end so a chatty adapter never blocks on a full pipe.
    while let Ok(read) = reader.read(&mut buf).await {
        if read == 0 {
            break;
        }
        let room = OUTPUT_KEEP_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..read.min(room)]);
    }
    kept
}

/// Kill the adapter's whole process group after a timeout, with the system
/// `kill` at a fixed path (no `PATH` lookup, no shell).
async fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let Some(kill) = ["/bin/kill", "/usr/bin/kill"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
        else {
            return;
        };
        let status = tokio::process::Command::new(kill)
            .args(["-s", "KILL", "--", &format!("-{pid}")])
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = tokio::time::timeout(Duration::from_secs(5), status).await;
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Run the adapter once and classify how it ended.
async fn execute(
    adapter: &WakeAdapter,
    program: &CheckedProgram,
    cwd: &Path,
    env: &[(OsString, OsString)],
    text: &str,
    timeout: Duration,
) -> WakeOutcome {
    let mut command = tokio::process::Command::new(&program.path);
    command
        .args(adapter.args(text))
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    WakeOutcome::Unavailable(DeliveryReason::BinaryMissing)
                }
                _ => WakeOutcome::Failed(DeliveryReason::AdapterExitNonzero),
            };
        }
    };
    let pid = child.id();
    let readers = [
        child
            .stdout
            .take()
            .map(|out| tokio::spawn(read_capped(out))),
        child
            .stderr
            .take()
            .map(|err| tokio::spawn(read_capped(err))),
    ];
    let run = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status
            .code()
            .map_or(AdapterRun::Signaled, AdapterRun::Exited),
        Ok(Err(_)) => AdapterRun::Signaled,
        Err(_) => {
            kill_group(pid).await;
            let _ = child.kill().await;
            AdapterRun::TimedOut
        }
    };
    let mut output = String::new();
    for mut reader in readers.into_iter().flatten() {
        match tokio::time::timeout(OUTPUT_GRACE, &mut reader).await {
            Ok(Ok(bytes)) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
                output.push('\n');
            }
            _ => reader.abort(),
        }
    }
    adapter.classify(run, &output)
}

#[derive(Debug, Deserialize)]
struct WatchReply {
    cursor: String,
    messages: Vec<WatchedMessage>,
}

#[derive(Debug, Deserialize)]
struct WatchedMessage {
    id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct CandidatesRequest<'a> {
    pub(super) workspace: &'a str,
    pub(super) project: &'a str,
    pub(super) agent: &'a str,
    pub(super) include_shared: bool,
    pub(super) wake_from: &'a [WakeFrom],
}

#[derive(Debug, Deserialize)]
pub(super) struct CandidatesReply {
    pub(super) messages: Vec<Candidate>,
    unresolved_wake_from: usize,
}

#[derive(Debug, Deserialize)]
pub(super) struct Candidate {
    pub(super) id: String,
    pub(super) sender: CandidateSender,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CandidateSender {
    SameProject,
    /// Another project joined to this one by an active link the destination
    /// confirmed: it wakes like the project's own mail.
    Linked,
    SameAgent,
    Listed,
    Unverified,
    Unlisted,
    System,
    /// The server's loop guard paused wakes between the sender and this
    /// agent. Never wakes; reported as `wake_failed/loop_suspected`.
    Paused,
    /// A label a newer server sends that this supervisor does not know.
    /// It never wakes.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Serialize)]
pub(super) struct ReportRequest<'a> {
    pub(super) workspace: &'a str,
    pub(super) project: &'a str,
    pub(super) agent: &'a str,
    pub(super) message_ids: &'a [String],
    pub(super) state: DeliveryState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<DeliveryReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) retry_after_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReportReply {
    updated: usize,
    receipts: usize,
}

/// Answer of `GET /inbox/bus`.
#[derive(Debug, Deserialize)]
struct BusReply {
    cursor: String,
    agents: Vec<BusAgent>,
}

/// One agent the bus reported: claimable mail addressed to it and the
/// newest wake handle of its harness in its project.
#[derive(Debug, Deserialize)]
struct BusAgent {
    workspace: String,
    project: String,
    agent: String,
    handle: BusHandle,
    pending: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BusHandle {
    kind: String,
    id: String,
    /// Whether the session is still open. A server that predates the field
    /// is read as open, which never resumes a session.
    #[serde(default = "open_by_default")]
    open: bool,
    #[serde(default)]
    cwd: Option<String>,
}

const fn open_by_default() -> bool {
    true
}

/// The messages of `candidates` that may wake `entry`, in server order:
/// another agent of the watched project (no authorization needed inside a
/// project), a listed sender whose session the server recorded, a listed
/// sender with no recorded session when the entry sets `wake_unverified`,
/// or a receipt when the adapter reaches a live session (owner decision Q6).
/// The agent's own mail to itself, an unlisted sender and an unknown label
/// never wake.
fn wakeable(entry: &Entry, candidates: &[Candidate], fresh: &[String]) -> Vec<String> {
    let fresh: HashSet<&str> = fresh.iter().map(String::as_str).collect();
    candidates
        .iter()
        .filter(|candidate| match candidate.sender {
            CandidateSender::SameProject | CandidateSender::Linked | CandidateSender::Listed => {
                true
            }
            CandidateSender::Unverified => entry.wake_unverified,
            CandidateSender::System => entry.adapter.wakes_live_session(),
            CandidateSender::SameAgent
            | CandidateSender::Unlisted
            | CandidateSender::Paused
            | CandidateSender::Unknown => false,
        })
        .filter_map(|candidate| canonical_message_id(&candidate.id))
        .filter(|id| fresh.contains(id.as_str()))
        .collect()
}

/// The fresh messages the server labelled `paused`: mail between two agents
/// its loop guard stopped waking.
fn paused_ids(candidates: &[Candidate], fresh: &[String]) -> Vec<String> {
    let fresh: HashSet<&str> = fresh.iter().map(String::as_str).collect();
    candidates
        .iter()
        .filter(|candidate| candidate.sender == CandidateSender::Paused)
        .filter_map(|candidate| canonical_message_id(&candidate.id))
        .filter(|id| fresh.contains(id.as_str()))
        .collect()
}

/// Largest `claude agents --json` answer read.
const PROBE_KEEP_BYTES: usize = 256 * 1024;
/// How long `claude agents --json` may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Ask `claude agents --json` whether a live process holds `session`, with
/// the same program, environment and protections as a wake. Anything but a
/// clean answer is [`ClaudeSessionPresence::Unknown`].
async fn claude_presence(
    program: &CheckedProgram,
    cwd: &Path,
    env: &[(OsString, OsString)],
    session: &str,
) -> ClaudeSessionPresence {
    let mut command = tokio::process::Command::new(&program.path);
    command
        .args(["agents", "--json"])
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let Ok(mut child) = command.spawn() else {
        return ClaudeSessionPresence::Unknown;
    };
    let pid = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        return ClaudeSessionPresence::Unknown;
    };
    let read = async {
        let mut kept = Vec::new();
        let mut buf = [0_u8; 8_192];
        while let Ok(n) = stdout.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let room = PROBE_KEEP_BYTES.saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
        let status = child.wait().await;
        (kept, status)
    };
    match tokio::time::timeout(PROBE_TIMEOUT, read).await {
        Ok((kept, Ok(status))) if status.success() && kept.len() < PROBE_KEEP_BYTES => {
            claude_session_presence(&String::from_utf8_lossy(&kept), session)
        }
        Ok(_) => ClaudeSessionPresence::Unknown,
        Err(_) => {
            kill_group(pid).await;
            ClaudeSessionPresence::Unknown
        }
    }
}

/// Check the directory a resumed session runs in: an existing absolute
/// directory owned by the operator, not writable by every user and not
/// under a temporary directory. A session's directory comes from its hook
/// events, so it is checked like a program before anything runs there.
fn check_session_dir(
    dir: &Path,
    owner: Option<u32>,
    temp_roots: &[PathBuf],
) -> Result<PathBuf, &'static str> {
    if !dir.is_absolute() {
        return Err("the session directory is not absolute");
    }
    let path = std::fs::canonicalize(dir).map_err(|_| "the session directory no longer exists")?;
    let meta = std::fs::metadata(&path).map_err(|_| "the session directory no longer exists")?;
    if !meta.is_dir() {
        return Err("the session directory is not a directory");
    }
    if temp_roots.iter().any(|root| path.starts_with(root)) {
        return Err("the session directory is under a temporary directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if meta.permissions().mode() & 0o002 != 0 {
            return Err("the session directory is world-writable");
        }
        if owner.is_some_and(|uid| meta.uid() != uid) {
            return Err("the session directory is owned by another user");
        }
    }
    #[cfg(not(unix))]
    let _ = owner;
    Ok(path)
}

/// The server answered 404: it predates the route.
fn not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ServerResponseError>())
        .is_some_and(|response| response.status() == reqwest::StatusCode::NOT_FOUND)
}

fn log(line: &str) {
    eprintln!("[ai-memory supervise {}] {line}", clock());
}

fn now_secs() -> i64 {
    jiff::Timestamp::now().as_second()
}

enum Stop {
    Done,
    AuthRefused,
}

/// The ledger and where it lives. Held only for bookkeeping, never while
/// an adapter runs.
struct LedgerFile {
    ledger: WakeLedger,
    path: PathBuf,
}

impl LedgerFile {
    fn save(&self) {
        if let Err(error) = save_json(&self.path, &self.ledger) {
            log(&format!(
                "could not save the wake ledger ({}); circuits may reset on restart",
                error.root_cause()
            ));
        }
    }
}

struct Supervisor {
    config: Config,
    endpoint: ServerEndpoint,
    entries: Vec<Entry>,
    cwd: PathBuf,
    approvals_path: PathBuf,
    owner: Option<u32>,
    temp_roots: Vec<PathBuf>,
    trust_on_first_use: bool,
    ledger: tokio::sync::Mutex<LedgerFile>,
    permits: tokio::sync::Semaphore,
    activity: std::sync::Mutex<Instant>,
    /// Discovered agents with a wake running or queued, so one agent never
    /// has two.
    in_flight: std::sync::Mutex<HashSet<String>>,
    /// Serializes changes to `approvals.json`.
    trust: tokio::sync::Mutex<()>,
}

/// Why a wake was not launched, already reported or logged.
enum Skip {
    /// Reported to the server (or a gate that reports nothing).
    Done,
}

impl Supervisor {
    fn touch(&self) {
        if let Ok(mut at) = self.activity.lock() {
            *at = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        self.activity
            .lock()
            .map_or(Duration::ZERO, |at| at.elapsed())
    }

    async fn report(&self, entry: &Entry, ids: &[String], outcome: WakeOutcome) {
        if ids.is_empty() {
            return;
        }
        let request = ReportRequest {
            workspace: &entry.workspace,
            project: &entry.project,
            agent: entry.agent.as_str(),
            message_ids: ids,
            state: outcome.state(),
            reason: outcome.reason(),
            retry_after_secs: outcome.retry_after_secs(),
        };
        let reply: Result<ReportReply> = post_json_bounded(
            &self.endpoint,
            "/inbox/wake-report",
            &request,
            REQUEST_TIMEOUT,
            MAX_REPLY_BYTES,
        )
        .await;
        let reason = outcome.reason().map_or("", DeliveryReason::as_str);
        match reply {
            Ok(reply) => log(&format!(
                "{}: reported {}{}{} for {} message(s); {} updated, {} receipt(s)",
                entry.name,
                outcome.state().as_str(),
                if reason.is_empty() { "" } else { "/" },
                reason,
                ids.len(),
                reply.updated,
                reply.receipts,
            )),
            Err(error) => log(&format!(
                "{}: could not report {} for {} message(s): {}",
                entry.name,
                outcome.state().as_str(),
                ids.len(),
                error.root_cause()
            )),
        }
    }

    /// Report a gate's outcome for the ids it has not reported yet.
    async fn report_once(&self, entry: &Entry, ids: &[String], outcome: WakeOutcome) -> bool {
        let fresh = {
            let mut file = self.ledger.lock().await;
            let fresh = file
                .ledger
                .take_unreported(&entry.name, ids, outcome.state(), now_secs());
            if !fresh.is_empty() {
                file.save();
            }
            fresh
        };
        self.report(entry, &fresh, outcome).await;
        !fresh.is_empty()
    }

    /// One look at an inbox: wake for the new mail that may wake the agent,
    /// or report why not.
    async fn handle(&self, entry: &Entry, pending: &[String]) -> Result<()> {
        let fresh = {
            let mut file = self.ledger.lock().await;
            let keep: BTreeSet<String> = pending.iter().cloned().collect();
            file.ledger.prune(&entry.name, &keep, now_secs());
            file.ledger.not_woken(&entry.name, pending)
        };
        if fresh.is_empty() {
            return Ok(());
        }
        self.touch();
        let candidates: CandidatesReply = post_json_bounded(
            &self.endpoint,
            "/inbox/wake-candidates",
            &CandidatesRequest {
                workspace: &entry.workspace,
                project: &entry.project,
                agent: entry.agent.as_str(),
                include_shared: entry.include_shared,
                wake_from: &entry.wake_from,
            },
            REQUEST_TIMEOUT,
            MAX_REPLY_BYTES,
        )
        .await?;
        if candidates.unresolved_wake_from > 0 {
            log(&format!(
                "{}: {} wake_from project(s) do not exist or cannot be read; they match nothing",
                entry.name, candidates.unresolved_wake_from
            ));
        }
        let paused = paused_ids(&candidates.messages, &fresh);
        if !paused.is_empty() {
            let outcome = WakeOutcome::Failed(DeliveryReason::LoopSuspected);
            if self.report_once(entry, &paused, outcome).await {
                log(&format!(
                    "{}: not woken for {} message(s): the server paused wakes between this agent \
                     and their sender (loop suspected); a human prompt in either session or the \
                     dashboard resumes them",
                    entry.name,
                    paused.len()
                ));
            }
        }
        let ids = wakeable(entry, &candidates.messages, &fresh);
        if ids.is_empty() {
            return Ok(());
        }
        // One agent's wakes run one at a time in its own task; the permit
        // bounds how many agents are woken at once.
        let _permit = self.permits.acquire().await?;
        let now = now_secs();
        let gate = self
            .ledger
            .lock()
            .await
            .ledger
            .gate(&entry.name, pending, now);
        match gate {
            WakeGate::Open => {}
            WakeGate::Backoff { .. } | WakeGate::Coalescing { .. } => return Ok(()),
            WakeGate::Quota { until, stated } => {
                let retry_after_secs = stated
                    .then(|| u64::try_from(until.saturating_sub(now)).ok())
                    .flatten()
                    .filter(|secs| *secs > 0);
                self.report_once(
                    entry,
                    &ids,
                    WakeOutcome::QuotaExhausted { retry_after_secs },
                )
                .await;
                return Ok(());
            }
        }
        let mut attempt = entry.clone();
        loop {
            let kind = attempt.adapter.kind();
            let blocked = self
                .ledger
                .lock()
                .await
                .ledger
                .auth_blocked_until(kind, now);
            let outcome = if blocked.is_some() {
                WakeOutcome::Unavailable(DeliveryReason::AuthFailed)
            } else {
                match self.launch(&attempt, &ids).await {
                    Ok(outcome) => outcome,
                    Err(Skip::Done) => return Ok(()),
                }
            };
            let next = matches!(
                outcome,
                WakeOutcome::Failed(DeliveryReason::SessionBusy | DeliveryReason::SessionNotFound)
                    | WakeOutcome::Unavailable(DeliveryReason::AuthFailed)
            )
            .then(|| kind.fallback())
            .flatten()
            .and_then(|fallback| attempt.with_adapter(fallback));
            if let Some(next) = next {
                log(&format!(
                    "{}: {} {}; trying {}",
                    entry.name,
                    kind.id(),
                    outcome
                        .reason()
                        .map_or(outcome.state().as_str(), DeliveryReason::as_str),
                    next.adapter.id()
                ));
                attempt = next;
                continue;
            }
            if blocked.is_some() {
                self.report_once(&attempt, &ids, outcome).await;
            } else {
                self.report(&attempt, &ids, outcome).await;
            }
            return Ok(());
        }
    }

    /// Run `entry`'s adapter once for `ids`, after its program is trusted
    /// and, for a Claude Code resume, after `claude agents --json` shows no
    /// live process for the session. Records the outcome in the ledger.
    async fn launch(&self, entry: &Entry, ids: &[String]) -> Result<WakeOutcome, Skip> {
        let program = self.trusted_program(entry, ids).await?;
        let kind = entry.adapter.kind();
        let env = wake_env(&entry.adapter, &program, &self.config.runtime_env);
        if kind == AdapterKind::ClaudeResume {
            let presence = claude_presence(&program, &self.cwd, &env, entry.adapter.target()).await;
            match presence {
                ClaudeSessionPresence::Absent => {}
                ClaudeSessionPresence::Live => {
                    log(&format!(
                        "{}: not resumed: a Claude Code process holds the session; its Stop hook \
                         wakes it",
                        entry.name
                    ));
                    return Err(Skip::Done);
                }
                ClaudeSessionPresence::Unknown => {
                    let outcome = WakeOutcome::Failed(DeliveryReason::SessionBusy);
                    if self.report_once(entry, ids, outcome).await {
                        log(&format!(
                            "{}: not resumed: `claude agents --json` did not answer, so the \
                             session may be live",
                            entry.name
                        ));
                    }
                    return Err(Skip::Done);
                }
            }
        }
        let run_dir = if kind.runs_in_session_dir() {
            let dir = entry
                .session
                .as_ref()
                .and_then(|session| session.cwd.clone());
            let (owner, roots) = (self.owner, self.temp_roots.clone());
            let checked = tokio::task::spawn_blocking(move || {
                dir.ok_or("the session has no recorded directory")
                    .and_then(|dir| check_session_dir(&dir, owner, &roots))
            })
            .await
            .unwrap_or(Err("the session directory could not be checked"));
            match checked {
                Ok(dir) => dir,
                Err(why) => {
                    let outcome = WakeOutcome::Failed(DeliveryReason::SessionNotFound);
                    // An adapter with a fallback that needs no directory (a
                    // stopped Codex conversation can still be queued) hands
                    // over instead of leaving the mail unannounced.
                    if kind.fallback().is_some() {
                        log(&format!("{}: not resumed: {why}", entry.name));
                        return Ok(outcome);
                    }
                    if self.report_once(entry, ids, outcome).await {
                        log(&format!("{}: not resumed: {why}", entry.name));
                    }
                    return Err(Skip::Done);
                }
            }
        } else {
            self.cwd.clone()
        };
        let Some(newest) = ids.last().and_then(|id| id.parse::<MessageId>().ok()) else {
            return Err(Skip::Done);
        };
        let text = wake_text(
            entry.agent,
            &entry.workspace,
            &entry.project,
            ids.len(),
            newest,
        );
        let outcome = execute(
            &entry.adapter,
            &program,
            &run_dir,
            &env,
            &text,
            entry.timeout,
        )
        .await;
        {
            let mut file = self.ledger.lock().await;
            file.ledger
                .record_outcome(&entry.name, kind, ids, outcome, now_secs());
            file.save();
        }
        log(&format!(
            "{}: {} wake for {} message(s), newest {newest}: {}",
            entry.name,
            entry.adapter.id(),
            ids.len(),
            outcome
                .reason()
                .map_or(outcome.state().as_str(), DeliveryReason::as_str),
        ));
        Ok(outcome)
    }

    /// Tell the server which program the supervisor now trusts, so the audit
    /// log and the dashboard show it. Best effort: an older server without
    /// the route, or one that is down, only costs the record.
    async fn report_program(
        &self,
        kind: AdapterKind,
        program: &CheckedProgram,
        previous_sha256: Option<&str>,
    ) {
        let body = serde_json::json!({
            "adapter": kind.id(),
            "program": program.path.to_string_lossy(),
            "sha256": program.sha256,
            "previous_sha256": previous_sha256,
            "trusted": if previous_sha256.is_some() { "changed" } else { "first_use" },
        });
        let sent: Result<serde_json::Value> = post_json_bounded(
            &self.endpoint,
            "/inbox/bus/program",
            &body,
            REQUEST_TIMEOUT,
            MAX_REPLY_BYTES,
        )
        .await;
        if let Err(error) = sent {
            // A 204 has no JSON body to read; only a refusal matters.
            if refused_credential(&error).is_some() || not_found(&error) {
                log(&format!(
                    "{} adapter: the server did not record the program ({})",
                    kind.id(),
                    error.root_cause()
                ));
            }
        }
    }

    /// The checked program `entry` runs. A configured entry needs its
    /// terminal approval. A discovered entry's adapter program is trusted on
    /// first use (or after a change of its binary) when the settings allow
    /// it, after the same file checks; otherwise it needs
    /// `message supervise approve --adapter`.
    async fn trusted_program(&self, entry: &Entry, ids: &[String]) -> Result<CheckedProgram, Skip> {
        let _trust = self.trust.lock().await;
        let mut approvals: Approvals = load_json(&self.config, &self.approvals_path);
        let kind = entry.adapter.kind();
        let recorded = approvals.program_for(entry);
        let configured = match (&recorded, entry.origin) {
            (Some(path), _) => Some(path.clone()),
            (None, Origin::Discovered) if self.trust_on_first_use => {
                locate_program(&self.config.runtime_env, kind, None)
            }
            (None, _) => None,
        };
        let Some(configured) = configured else {
            let outcome = if entry.origin == Origin::Discovered && self.trust_on_first_use {
                WakeOutcome::Unavailable(DeliveryReason::BinaryMissing)
            } else {
                WakeOutcome::Failed(DeliveryReason::NotApproved)
            };
            if self.report_once(entry, ids, outcome).await {
                log(&format!(
                    "{}: not woken: no {} program was found for the {} adapter{}",
                    entry.name,
                    kind.program_name(),
                    kind.id(),
                    if self.trust_on_first_use {
                        " on PATH"
                    } else {
                        "; run `ai-memory message supervise approve --adapter` in a terminal"
                    }
                ));
            }
            return Err(Skip::Done);
        };
        let (owner, roots) = (self.owner, self.temp_roots.clone());
        let program =
            tokio::task::spawn_blocking(move || check_program(&configured, owner, &roots))
                .await
                .unwrap_or(Err(ProgramProblem::Missing));
        let program = match program {
            Ok(program) => program,
            Err(problem) => {
                if self.report_once(entry, ids, problem.outcome()).await {
                    log(&format!(
                        "{}: not woken: {}",
                        entry.name,
                        problem.describe()
                    ));
                }
                return Err(Skip::Done);
            }
        };
        if approvals.approves_entry(entry, &program, &self.cwd) {
            return Ok(program);
        }
        if entry.origin == Origin::Discovered && self.trust_on_first_use {
            let previous = approvals.programs.get(kind.id()).cloned();
            let changed = previous.is_some();
            let previous_sha = previous.as_ref().map(|approval| approval.sha256.clone());
            approvals.programs.insert(
                kind.id().to_owned(),
                Approval {
                    digest: program_digest(kind, &program, &self.cwd),
                    program: program.path.clone(),
                    sha256: program.sha256.clone(),
                    approved_at: jiff::Timestamp::now().to_string(),
                    trusted: Some(if changed { "changed" } else { "first_use" }.to_owned()),
                    previous_sha256: previous.map(|approval| approval.sha256),
                },
            );
            if let Err(error) = save_json(&self.approvals_path, &approvals) {
                log(&format!(
                    "{}: could not record trust in {} ({})",
                    entry.name,
                    kind.program_name(),
                    error.root_cause()
                ));
            }
            self.report_program(
                kind,
                &program,
                changed.then_some(previous_sha.as_deref()).flatten(),
            )
            .await;
            log(&format!(
                "{} adapter: {} {} (sha256 {})",
                kind.id(),
                if changed {
                    "the program changed; trusting the new binary at"
                } else {
                    "trusting on first use"
                },
                program.path.display(),
                program.sha256
            ));
            return Ok(program);
        }
        let outcome = WakeOutcome::Failed(DeliveryReason::NotApproved);
        if self.report_once(entry, ids, outcome).await {
            let command = match entry.origin {
                Origin::Configured => format!("approve {}", entry.name),
                Origin::Discovered => format!("approve --adapter {}", kind.id()),
            };
            log(&format!(
                "{}: not woken: the adapter is not approved, or its program or entry changed \
                 since; run `ai-memory message supervise {command}` in a terminal",
                entry.name
            ));
        }
        Err(Skip::Done)
    }

    /// Whether a configured entry already supervises this inbox and harness;
    /// the bus then leaves it to that entry.
    fn configured_for(&self, target: &BusAgent) -> bool {
        self.entries.iter().any(|entry| {
            entry.agent.as_str() == target.agent
                && entry.workspace == target.workspace.trim()
                && entry.project == target.project.trim()
        })
    }

    /// Handle one discovered agent in its own task unless a wake for it is
    /// already running.
    fn spawn_discovered(
        self: &Arc<Self>,
        entry: Entry,
        pending: Vec<String>,
        tasks: &mut tokio::task::JoinSet<()>,
    ) {
        let fresh = self
            .in_flight
            .lock()
            .is_ok_and(|mut running| running.insert(entry.name.clone()));
        if !fresh {
            return;
        }
        let supervisor = Arc::clone(self);
        tasks.spawn(async move {
            if let Err(error) = supervisor.handle(&entry, &pending).await {
                log(&format!(
                    "{}: could not handle its mail: {}",
                    entry.name,
                    error.root_cause()
                ));
            }
            if let Ok(mut running) = supervisor.in_flight.lock() {
                running.remove(&entry.name);
            }
        });
    }

    /// Long-poll `GET /inbox/bus` and wake the discovered agents it lists.
    /// A refused credential stops the supervisor; a server that does not know
    /// the route, or a credential that is not root, turns discovery off and
    /// leaves the configured entries running.
    async fn bus(self: Arc<Self>, once: bool) -> Result<Stop> {
        let mut cursor: Option<String> = None;
        let mut failing = false;
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            while tasks.try_join_next().is_some() {}
            let wait = if once || cursor.is_none() {
                Duration::ZERO
            } else {
                WATCH_WAIT
            };
            let wait_ms = wait.as_millis().to_string();
            let mut query = vec![("wait_ms", wait_ms.as_str())];
            if let Some(after) = cursor.as_deref() {
                query.push(("after", after));
            }
            let reply: Result<BusReply> = get_json_bounded(
                &self.endpoint,
                "/inbox/bus",
                &query,
                wait + REPLY_GRACE,
                MAX_REPLY_BYTES,
            )
            .await;
            match reply {
                Ok(reply) => {
                    if failing {
                        log("agent bus: the server answers again");
                        failing = false;
                    }
                    cursor = valid_cursor(&reply.cursor).map(str::to_owned);
                    for target in &reply.agents {
                        if self.configured_for(target) {
                            continue;
                        }
                        let Some(entry) = Entry::discovered(target) else {
                            continue;
                        };
                        let pending: Vec<String> = target
                            .pending
                            .iter()
                            .filter_map(|id| canonical_message_id(id))
                            .collect();
                        self.spawn_discovered(entry, pending, &mut tasks);
                    }
                }
                Err(error) => {
                    if refused_credential(&error).is_some() {
                        if refused_credential(&error) == Some(403) {
                            log(
                                "agent bus: the server refused to list every project (403); \
                                 discovery needs the root credential and is off",
                            );
                            return Ok(Stop::Done);
                        }
                        log("agent bus: authentication refused; the supervisor stops");
                        return Ok(Stop::AuthRefused);
                    }
                    if not_found(&error) {
                        log("agent bus: the server has no /inbox/bus route; discovery is off");
                        return Ok(Stop::Done);
                    }
                    if once || !transient(&error) {
                        return Err(error);
                    }
                    if !failing {
                        log("agent bus: the ai-memory server is unavailable; will keep trying");
                        failing = true;
                    }
                    cursor = None;
                    tokio::time::sleep(WATCH_WAIT).await;
                }
            }
            if once {
                while tasks.join_next().await.is_some() {}
                return Ok(Stop::Done);
            }
        }
    }

    async fn watch(self: Arc<Self>, index: usize, once: bool) -> Result<Stop> {
        let entry = &self.entries[index];
        let include_shared = if entry.include_shared {
            "true"
        } else {
            "false"
        };
        let mut cursor: Option<String> = None;
        let mut failing = false;
        loop {
            let wait = if once || cursor.is_none() {
                Duration::ZERO
            } else {
                WATCH_WAIT
            };
            let wait_ms = wait.as_millis().to_string();
            let mut query = vec![
                ("workspace", entry.workspace.as_str()),
                ("project", entry.project.as_str()),
                ("agent", entry.agent.as_str()),
                ("include_shared", include_shared),
                ("wait_ms", wait_ms.as_str()),
            ];
            if let Some(after) = cursor.as_deref() {
                query.push(("after", after));
            }
            let reply: Result<WatchReply> = get_json_bounded(
                &self.endpoint,
                "/inbox/watch",
                &query,
                wait + REPLY_GRACE,
                MAX_REPLY_BYTES,
            )
            .await;
            let outcome = match reply {
                Ok(reply) => {
                    cursor = valid_cursor(&reply.cursor).map(str::to_owned);
                    let pending: Vec<String> = reply
                        .messages
                        .iter()
                        .filter_map(|message| canonical_message_id(&message.id))
                        .collect();
                    self.handle(entry, &pending).await
                }
                Err(error) => Err(error),
            };
            match outcome {
                Ok(()) => {
                    if failing {
                        log(&format!("{}: the server answers again", entry.name));
                        failing = false;
                    }
                }
                Err(error) => {
                    if refused_credential(&error).is_some() {
                        log(&format!(
                            "{}: authentication refused; the supervisor stops",
                            entry.name
                        ));
                        return Ok(Stop::AuthRefused);
                    }
                    if once || !transient(&error) {
                        return Err(error);
                    }
                    if !failing {
                        log(&format!(
                            "{}: the ai-memory server is unavailable; will keep trying",
                            entry.name
                        ));
                        failing = true;
                    }
                    cursor = None;
                    tokio::time::sleep(WATCH_WAIT).await;
                }
            }
            if once {
                return Ok(Stop::Done);
            }
        }
    }
}

async fn supervise(config: &Config, state_dir: &Path, once: bool) -> Result<i32> {
    let entries = entries(config)?;
    let owner = prepare_state_dir(state_dir)?;
    let lock_path = state_dir.join("supervisor.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening the supervisor lock {}", lock_path.display()))?;
    if lock.try_lock_exclusive().is_err() {
        log("another supervisor already uses this state directory; this one did not start");
        return Ok(EXIT_ALREADY_RUNNING);
    }
    let cwd = std::fs::canonicalize(state_dir).with_context(|| {
        format!(
            "creating the supervisor state directory {}",
            state_dir.display()
        )
    })?;
    let approvals_path = cwd.join("approvals.json");
    let roots = temp_roots();
    if !once && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        for entry in &entries {
            let Ok(program) = check_program(&entry.program, owner, &roots) else {
                continue;
            };
            let approvals: Approvals = load_json(config, &approvals_path);
            if !approvals.approves(&entry.name, &approval_digest(entry, &program, &cwd)) {
                let mut input = std::io::stdin().lock();
                let mut output = std::io::stdout().lock();
                approve_interactive(
                    config,
                    entry,
                    &program,
                    &cwd,
                    &approvals_path,
                    &mut input,
                    &mut output,
                )?;
            }
        }
    }
    let ledger_path = cwd.join("ledger.json");
    let settings = &config.messaging.supervisor;
    let supervisor = Arc::new(Supervisor {
        config: config.clone(),
        endpoint: ServerEndpoint::from_config_resolving_auth(config).await,
        cwd,
        approvals_path,
        owner,
        temp_roots: roots,
        trust_on_first_use: settings.trust_on_first_use,
        ledger: tokio::sync::Mutex::new(LedgerFile {
            ledger: load_json(config, &ledger_path),
            path: ledger_path,
        }),
        permits: tokio::sync::Semaphore::new(
            usize::try_from(settings.max_concurrent_wakes).unwrap_or(1),
        ),
        activity: std::sync::Mutex::new(Instant::now()),
        in_flight: std::sync::Mutex::new(HashSet::new()),
        trust: tokio::sync::Mutex::new(()),
        entries,
    });
    log(&format!(
        "watching {} agent(s): {}{}",
        supervisor.entries.len(),
        supervisor
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        if settings.discover {
            "; discovering the agents hook sessions report"
        } else {
            ""
        }
    ));
    let idle = (settings.idle_exit_minutes > 0)
        .then(|| Duration::from_secs(u64::from(settings.idle_exit_minutes) * 60));
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..supervisor.entries.len() {
        tasks.spawn(supervisor.clone().watch(index, once));
    }
    if settings.discover {
        tasks.spawn(supervisor.clone().bus(once));
    }
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    let code = loop {
        tokio::select! {
            joined = tasks.join_next() => match joined {
                None => break 0,
                Some(Ok(Ok(Stop::Done))) => {}
                Some(Ok(Ok(Stop::AuthRefused))) => break EXIT_AUTH_REFUSED,
                Some(Ok(Err(error))) => {
                    tasks.abort_all();
                    drop(lock);
                    return Err(error);
                }
                Some(Err(error)) => {
                    tasks.abort_all();
                    drop(lock);
                    return Err(anyhow::Error::from(error));
                }
            },
            _ = tick.tick(), if idle.is_some() => {
                if idle.is_some_and(|idle| supervisor.idle_for() >= idle) {
                    log("no mail and no wake for the configured idle time; stopping");
                    break 0;
                }
            }
        }
    };
    tasks.abort_all();
    drop(lock);
    Ok(code)
}

/// Show the operator exactly what an approval allows and ask on the
/// terminal. Records the approval only on an explicit yes.
fn approve_interactive(
    config: &Config,
    entry: &Entry,
    program: &CheckedProgram,
    cwd: &Path,
    approvals_path: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool> {
    let i18n = &config.i18n;
    let text = wake_text(
        entry.agent,
        &entry.workspace,
        &entry.project,
        1,
        EXAMPLE_ID.parse().unwrap_or_default(),
    );
    let mut argv = vec![program.path.to_string_lossy().into_owned()];
    argv.extend(entry.adapter.args(&text));
    let env_keys: Vec<&str> = PASSTHROUGH_ENV
        .iter()
        .chain(entry.adapter.harness_env())
        .copied()
        .chain(std::iter::once("PATH"))
        .collect();
    let wake_from = if entry.wake_from.is_empty() {
        "no other project".to_owned()
    } else {
        entry
            .wake_from
            .iter()
            .map(|scope| format!("{}/{}", scope.workspace, scope.project))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let unverified = if entry.wake_unverified {
        "  wake_unverified: true (a listed project wakes it without a recorded sender session)"
    } else {
        "  wake_unverified: false (a listed project wakes it only when the server recorded the \
         sender's session)"
    };
    let risk = if entry.wake_unverified {
        "  any local process with the server token can wake this agent through mail sent as \
         its own project or a listed one; the wake text is fixed and the wake limits still apply"
    } else {
        "  any local process with the server token can wake this agent through mail sent as \
         its own project; the wake text is fixed and the wake limits still apply"
    };
    let lines = [
        crate::localization::diagnostic(
            i18n,
            format!(
                "Approve wake adapter {} for supervised agent {} ({} in {}/{})?",
                entry.adapter.id(),
                entry.name,
                entry.agent.as_str(),
                super::show::terminal_text(&entry.workspace),
                super::show::terminal_text(&entry.project),
            ),
        ),
        crate::localization::diagnostic(
            i18n,
            format!(
                "  program: {} (sha256 {})",
                program.path.display(),
                program.sha256
            ),
        ),
        crate::localization::diagnostic(
            i18n,
            format!(
                "  argv: {}",
                super::show::terminal_text(&format!("{argv:?}"))
            ),
        ),
        crate::localization::diagnostic(i18n, format!("  working directory: {}", cwd.display())),
        crate::localization::diagnostic(i18n, format!("  environment: {}", env_keys.join(", "))),
        crate::localization::diagnostic(
            i18n,
            format!(
                "  own project: other agents of {}/{} wake it with no wake_from entry or recorded session",
                super::show::terminal_text(&entry.workspace),
                super::show::terminal_text(&entry.project),
            ),
        ),
        crate::localization::diagnostic(
            i18n,
            format!("  woken by: {}", super::show::terminal_text(&wake_from)),
        ),
        crate::localization::diagnostic(i18n, unverified),
        crate::localization::diagnostic(i18n, risk),
    ];
    for line in &lines {
        writeln!(output, "{line}")?;
    }
    write!(
        output,
        "{}",
        crate::localization::diagnostic(i18n, "Approve? [y/N] ".to_owned())
    )?;
    output.flush()?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .context("reading the approval answer")?;
    let yes = matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "s" | "sim"
    );
    if !yes {
        writeln!(
            output,
            "{}",
            crate::localization::diagnostic(i18n, "Not approved; nothing was recorded.".to_owned())
        )?;
        return Ok(false);
    }
    let mut approvals: Approvals = load_json(config, approvals_path);
    approvals.entries.insert(
        entry.name.clone(),
        Approval {
            digest: approval_digest(entry, program, cwd),
            program: program.path.clone(),
            sha256: program.sha256.clone(),
            approved_at: jiff::Timestamp::now().to_string(),
            trusted: Some("terminal".to_owned()),
            previous_sha256: None,
        },
    );
    save_json(approvals_path, &approvals)
        .with_context(|| format!("saving the approvals in {}", approvals_path.display()))?;
    writeln!(
        output,
        "{}",
        crate::localization::diagnostic(
            i18n,
            format!("Approved {}; the supervisor will run it.", entry.name)
        )
    )?;
    Ok(true)
}

fn approve_command(config: &Config, state_dir: &Path, name: &str) -> Result<i32> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "approving a wake adapter needs a terminal; run `ai-memory message supervise approve {}` in one",
            super::show::terminal_text(name)
        );
    }
    let entries = entries(config)?;
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
        anyhow::bail!(
            "no supervised agent is named {}",
            super::show::terminal_text(name)
        );
    };
    let owner = prepare_state_dir(state_dir)?;
    let cwd = std::fs::canonicalize(state_dir).with_context(|| {
        format!(
            "creating the supervisor state directory {}",
            state_dir.display()
        )
    })?;
    let program = check_program(&entry.program, owner, &temp_roots()).map_err(|problem| {
        anyhow::anyhow!(
            "the program of {} cannot be approved: {}",
            entry.name,
            problem.describe()
        )
    })?;
    let approved = approve_interactive(
        config,
        entry,
        &program,
        &cwd,
        &cwd.join("approvals.json"),
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )?;
    Ok(if approved { 0 } else { 1 })
}

/// The program to approve for `kind`: the one named, or the first executable
/// file called [`AdapterKind::program_name`] on `PATH`.
/// The program a discovered agent's adapter would be trusted with, as the
/// supervisor looks for it.
pub(super) fn find_program(env: &RuntimeEnv, kind: AdapterKind) -> Option<PathBuf> {
    locate_program(env, kind, None)
}

/// Where a harness installs its command line besides `PATH`, under the
/// operator's home: a user service starts with a short `PATH`.
const HOME_BIN_DIRS: &[&str] = &[".local/bin", "bin", ".npm-global/bin", ".bun/bin"];
/// System directories searched after `PATH`.
const EXTRA_BIN_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

fn locate_program(env: &RuntimeEnv, kind: AdapterKind, named: Option<&Path>) -> Option<PathBuf> {
    if let Some(named) = named {
        return Some(named.to_path_buf());
    }
    let path_dirs = env
        .var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let home_dirs = env
        .var_os("HOME")
        .map(PathBuf::from)
        .into_iter()
        .flat_map(|home| HOME_BIN_DIRS.iter().map(move |dir| home.join(dir)));
    path_dirs
        .into_iter()
        .chain(home_dirs)
        .chain(EXTRA_BIN_DIRS.iter().map(PathBuf::from))
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(kind.program_name()))
        .find(|candidate| candidate.is_file())
}

/// Show the operator what a program approval allows for every discovered
/// agent of one adapter kind and ask on the terminal. Records the approval
/// only on an explicit yes.
fn approve_program_interactive(
    config: &Config,
    kind: AdapterKind,
    program: &CheckedProgram,
    cwd: &Path,
    approvals_path: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool> {
    let i18n = &config.i18n;
    let mut argv = vec![program.path.to_string_lossy().into_owned()];
    argv.extend(
        kind.argv_shape()
            .into_iter()
            .map(|part| match part.as_str() {
                "{handle}" => EXAMPLE_HANDLE.to_owned(),
                "{wake}" => wake_text(
                    kind.agent(),
                    "<workspace>",
                    "<project>",
                    1,
                    EXAMPLE_ID.parse().unwrap_or_default(),
                ),
                _ => part,
            }),
    );
    let env_keys: Vec<&str> = PASSTHROUGH_ENV
        .iter()
        .chain(kind.harness_env())
        .copied()
        .chain(std::iter::once("PATH"))
        .collect();
    let lines = [
        crate::localization::diagnostic(
            i18n,
            format!(
                "Approve the program of wake adapter {} for every discovered agent?",
                kind.id()
            ),
        ),
        crate::localization::diagnostic(
            i18n,
            format!(
                "  program: {} (sha256 {})",
                program.path.display(),
                program.sha256
            ),
        ),
        crate::localization::diagnostic(
            i18n,
            format!(
                "  argv: {}",
                super::show::terminal_text(&format!("{argv:?}"))
            ),
        ),
        crate::localization::diagnostic(i18n, format!("  working directory: {}", cwd.display())),
        crate::localization::diagnostic(i18n, format!("  environment: {}", env_keys.join(", "))),
        crate::localization::diagnostic(
            i18n,
            "  targets: each session a hook reported for this harness, by its session id; \
             only mail from another agent of the same project wakes it"
                .to_owned(),
        ),
        crate::localization::diagnostic(
            i18n,
            "  any local process with the server token can wake these agents through mail sent \
             as their own project; the wake text is fixed and the wake limits still apply"
                .to_owned(),
        ),
    ];
    for line in &lines {
        writeln!(output, "{line}")?;
    }
    write!(
        output,
        "{}",
        crate::localization::diagnostic(i18n, "Approve? [y/N] ".to_owned())
    )?;
    output.flush()?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .context("reading the approval answer")?;
    let yes = matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "s" | "sim"
    );
    if !yes {
        writeln!(
            output,
            "{}",
            crate::localization::diagnostic(i18n, "Not approved; nothing was recorded.".to_owned())
        )?;
        return Ok(false);
    }
    let mut approvals: Approvals = load_json(config, approvals_path);
    approvals.programs.insert(
        kind.id().to_owned(),
        Approval {
            digest: program_digest(kind, program, cwd),
            program: program.path.clone(),
            sha256: program.sha256.clone(),
            approved_at: jiff::Timestamp::now().to_string(),
            trusted: Some("terminal".to_owned()),
            previous_sha256: None,
        },
    );
    save_json(approvals_path, &approvals)
        .with_context(|| format!("saving the approvals in {}", approvals_path.display()))?;
    writeln!(
        output,
        "{}",
        crate::localization::diagnostic(
            i18n,
            format!("Approved {}; the supervisor will run it.", kind.id())
        )
    )?;
    Ok(true)
}

fn approve_program_command(
    config: &Config,
    state_dir: &Path,
    adapter: &str,
    named: Option<&Path>,
) -> Result<i32> {
    let Some(kind) = AdapterKind::parse(adapter) else {
        anyhow::bail!(
            "unknown adapter {}; the adapters are: {}",
            super::show::terminal_text(adapter),
            AdapterKind::ALL.map(AdapterKind::id).join(", ")
        );
    };
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "approving a wake adapter needs a terminal; run `ai-memory message supervise approve --adapter {}` in one",
            kind.id()
        );
    }
    let Some(configured) = locate_program(&config.runtime_env, kind, named) else {
        anyhow::bail!(
            "no {} program was found on PATH; pass its absolute path with --program",
            kind.program_name()
        );
    };
    let owner = prepare_state_dir(state_dir)?;
    let cwd = std::fs::canonicalize(state_dir).with_context(|| {
        format!(
            "creating the supervisor state directory {}",
            state_dir.display()
        )
    })?;
    let program = check_program(&configured, owner, &temp_roots()).map_err(|problem| {
        anyhow::anyhow!(
            "the program of {} cannot be approved: {}",
            kind.id(),
            problem.describe()
        )
    })?;
    let approved = approve_program_interactive(
        config,
        kind,
        &program,
        &cwd,
        &cwd.join("approvals.json"),
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )?;
    Ok(if approved { 0 } else { 1 })
}

fn unit_command(config: &Config, config_path: Option<&Path>) -> Result<i32> {
    let Some(manager) = ServiceManager::current() else {
        anyhow::bail!("the messaging supervisor has no Windows service; run it from a terminal");
    };
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("locating the running ai-memory binary")?;
    let home = super::run::native_home(config).context("locating your home directory")?;
    let spec = ServiceSpec {
        exe,
        data_dir: config.data_dir.clone(),
        config_path: config_path
            .map_or_else(|| config.data_dir.join("config.toml"), Path::to_path_buf),
        home,
        xdg_config_home: config
            .runtime_env
            .var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from),
    };
    let unit = user_service::render_supervisor(manager, &spec)?;
    print!("{}", unit.contents);
    let enable = match manager {
        ServiceManager::SystemdUser => format!(
            "systemctl --user daemon-reload && systemctl --user enable --now {}",
            user_service::SUPERVISOR_SYSTEMD_UNIT_NAME
        ),
        _ => format!("launchctl bootstrap gui/$(id -u) {}", unit.path.display()),
    };
    eprintln!(
        "{}",
        crate::localization::diagnostic(
            &config.i18n,
            format!(
                "Nothing was installed. To run the supervisor as a service, save this as {} and run: {enable}",
                unit.path.display()
            )
        )
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718293";

    fn entry(program: &Path) -> Entry {
        Entry {
            name: "codex-linha".into(),
            origin: Origin::Configured,
            agent: AgentKind::Codex,
            workspace: "default".into(),
            project: "linha".into(),
            include_shared: false,
            wake_from: vec![WakeFrom {
                workspace: "default".into(),
                project: "fortaleza".into(),
            }],
            wake_unverified: false,
            adapter: WakeAdapter::CodexQueue {
                conversation: ai_memory_core::message_wake::ConversationRef::parse(ID).unwrap(),
            },
            program: program.to_path_buf(),
            session: None,
            timeout: Duration::from_secs(60),
        }
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_or_missing_programs_are_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let good = script(&dir, "codex", "exit 0", 0o755);
        let checked = check_program(&good, None, &[]).unwrap();
        assert_eq!(checked.path, std::fs::canonicalize(&good).unwrap());
        assert_eq!(checked.sha256.len(), 64);

        assert_eq!(
            check_program(&dir.join("absent"), None, &[]),
            Err(ProgramProblem::Missing)
        );
        assert!(matches!(
            check_program(Path::new("codex"), None, &[]),
            Err(ProgramProblem::Unsafe(_))
        ));
        let writable = script(&dir, "writable", "exit 0", 0o777);
        assert!(matches!(
            check_program(&writable, None, &[]),
            Err(ProgramProblem::Unsafe(
                "the program is group- or world-writable"
            ))
        ));
        let plain = script(&dir, "plain", "exit 0", 0o644);
        assert!(matches!(
            check_program(&plain, None, &[]),
            Err(ProgramProblem::Unsafe("the program is not executable"))
        ));
        assert!(matches!(
            check_program(&dir, None, &[]),
            Err(ProgramProblem::Unsafe("the program is not a regular file"))
        ));
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(matches!(
            check_program(&good, None, &[root]),
            Err(ProgramProblem::Unsafe(
                "the program is under a temporary directory"
            ))
        ));
        use std::os::unix::fs::MetadataExt as _;
        let mine = std::fs::metadata(&good).unwrap().uid();
        if mine != 0 {
            assert!(matches!(
                check_program(&good, Some(mine.wrapping_add(1)), &[]),
                Err(ProgramProblem::Unsafe(
                    "the program is owned by another user"
                ))
            ));
        }
        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        let inside = script(&open, "codex", "exit 0", 0o755);
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            check_program(&inside, None, &[]),
            Err(ProgramProblem::Unsafe(
                "the program is in a world-writable directory"
            ))
        ));
    }

    fn bus_agent(agent: &str, kind: &str, id: &str) -> BusAgent {
        bus_session(agent, kind, id, true)
    }

    fn bus_session(agent: &str, kind: &str, id: &str, open: bool) -> BusAgent {
        BusAgent {
            workspace: "default".into(),
            project: "linha".into(),
            agent: agent.into(),
            handle: BusHandle {
                kind: kind.into(),
                id: id.into(),
                open,
                cwd: Some("/home/someone/linha".into()),
            },
            pending: Vec::new(),
        }
    }

    #[test]
    fn a_discovered_entry_needs_a_matching_harness_handle_and_adapter() {
        let entry = Entry::discovered(&bus_agent("codex", "codex_thread", ID)).unwrap();
        assert_eq!(entry.origin, Origin::Discovered);
        assert_eq!(entry.name, "default/linha/codex");
        assert!(entry.wake_from.is_empty(), "only its own project wakes it");
        assert!(!entry.wake_unverified && !entry.include_shared);
        assert_eq!(
            entry.adapter.args("T"),
            ["queue", "--thread", ID, "--message", "T"]
        );
        // A stopped Codex conversation is resumed headless, with the queue
        // as its fallback.
        let stopped = Entry::discovered(&bus_session("codex", "codex_thread", ID, false)).unwrap();
        assert_eq!(stopped.adapter.kind(), AdapterKind::CodexExecResume);
        let queued = stopped.with_adapter(AdapterKind::CodexQueue).unwrap();
        assert_eq!(queued.adapter.args("T")[0], "queue");
        assert_eq!(queued.timeout, Duration::from_secs(60));
        // A stopped Claude Code session is resumed in its own directory.
        let claude =
            Entry::discovered(&bus_session("claude-code", "claude_session", ID, false)).unwrap();
        assert_eq!(claude.adapter.kind(), AdapterKind::ClaudeResume);
        assert_eq!(
            claude.session.as_ref().and_then(|s| s.cwd.clone()),
            Some(PathBuf::from("/home/someone/linha"))
        );
        let gemini =
            Entry::discovered(&bus_session("gemini-cli", "gemini_session", ID, false)).unwrap();
        assert_eq!(gemini.adapter.kind(), AdapterKind::GeminiResume);
        for refused in [
            // An open Claude Code or Gemini CLI session is woken by its own
            // hook, never by a resume.
            bus_agent("claude-code", "claude_session", ID),
            bus_agent("gemini-cli", "gemini_session", ID),
            // A handle kind that does not belong to the harness.
            bus_agent("codex", "claude_session", ID),
            bus_agent("claude-code", "codex_thread", ID),
            // Unknown or alias names, and handles that are not canonical UUIDs.
            bus_agent("claude", "claude_session", ID),
            bus_agent("codex", "shell", ID),
            bus_agent("codex", "codex_thread", "--resume"),
            bus_agent("codex", "codex_thread", "work-chat"),
        ] {
            assert!(Entry::discovered(&refused).is_none(), "{refused:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_session_runs_only_in_a_checked_directory_of_the_operator() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("linha");
        std::fs::create_dir(&project).unwrap();
        let uid = std::fs::metadata(&project).unwrap().uid();
        assert_eq!(
            check_session_dir(&project, Some(uid), &[]).unwrap(),
            std::fs::canonicalize(&project).unwrap()
        );
        assert!(check_session_dir(Path::new("linha"), Some(uid), &[]).is_err());
        assert!(check_session_dir(&tmp.path().join("gone"), Some(uid), &[]).is_err());
        assert!(check_session_dir(&project, Some(uid.wrapping_add(1)), &[]).is_err());
        let temp = vec![std::fs::canonicalize(tmp.path()).unwrap()];
        assert!(check_session_dir(&project, Some(uid), &temp).is_err());
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_session_dir(&project, Some(uid), &[]).is_err());
    }

    #[test]
    fn a_service_finds_a_harness_in_the_home_bin_dirs_its_path_lacks() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude"), b"").unwrap();
        let env = RuntimeEnv::with_process_env_for_tests(&[
            ("PATH", "/nonexistent-dir"),
            ("HOME", tmp.path().to_str().unwrap()),
        ]);
        assert_eq!(
            locate_program(&env, AdapterKind::ClaudeResume, None),
            Some(bin.join("claude"))
        );
    }

    #[test]
    fn paused_mail_is_reported_and_never_wakes() {
        let entry = Entry::discovered(&bus_agent("codex", "codex_thread", ID)).unwrap();
        let other = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718294".to_owned();
        let candidates = vec![
            Candidate {
                id: ID.into(),
                sender: CandidateSender::Paused,
            },
            Candidate {
                id: other.clone(),
                sender: CandidateSender::SameProject,
            },
        ];
        let fresh = vec![ID.to_owned(), other.clone()];
        assert_eq!(paused_ids(&candidates, &fresh), [ID]);
        assert_eq!(wakeable(&entry, &candidates, &fresh), [other]);
    }

    #[test]
    fn a_program_approval_covers_one_adapter_and_one_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let program = script(tmp.path(), "codex", "exit 0", 0o755);
        let cwd = tmp.path().to_path_buf();
        let checked = check_program(&program, None, &[]).unwrap();
        let digest = program_digest(AdapterKind::CodexQueue, &checked, &cwd);
        let mut approvals = Approvals::default();
        approvals.programs.insert(
            "codex_queue".into(),
            Approval {
                digest: digest.clone(),
                program: checked.path.clone(),
                sha256: checked.sha256.clone(),
                approved_at: String::new(),
                trusted: None,
                previous_sha256: None,
            },
        );
        let discovered = Entry::discovered(&bus_agent("codex", "codex_thread", ID)).unwrap();
        assert_eq!(
            approvals.program_for(&discovered),
            Some(checked.path.clone())
        );
        assert!(approvals.approves_entry(&discovered, &checked, &cwd));
        // A program approval never approves a configured entry, nor does an
        // entry approval cover discovered agents.
        let configured = entry(&program);
        assert!(!approvals.approves_entry(&configured, &checked, &cwd));
        let mut by_name = Approvals::default();
        by_name.entries.insert(
            "codex-linha".into(),
            Approval {
                digest: approval_digest(&configured, &checked, &cwd),
                program: checked.path.clone(),
                sha256: checked.sha256.clone(),
                approved_at: String::new(),
                trusted: None,
                previous_sha256: None,
            },
        );
        assert!(by_name.approves_entry(&configured, &checked, &cwd));
        assert!(!by_name.approves_entry(&discovered, &checked, &cwd));
        assert_eq!(by_name.program_for(&discovered), None);
        // A changed binary voids it.
        script(tmp.path(), "codex", "exit 0 # changed", 0o755);
        let changed = check_program(&program, None, &[]).unwrap();
        assert!(!approvals.approves_entry(&discovered, &changed, &cwd));
    }

    #[test]
    fn the_approval_is_pinned_to_the_binary_content_and_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let program = script(tmp.path(), "codex", "exit 0", 0o755);
        let cwd = tmp.path().to_path_buf();
        let base = entry(&program);
        let first = approval_digest(&base, &check_program(&program, None, &[]).unwrap(), &cwd);
        assert_eq!(
            first,
            approval_digest(&base, &check_program(&program, None, &[]).unwrap(), &cwd),
            "stable while nothing changes"
        );
        script(tmp.path(), "codex", "exit 0 # changed", 0o755);
        assert_ne!(
            first,
            approval_digest(&base, &check_program(&program, None, &[]).unwrap(), &cwd),
            "a changed binary voids the approval"
        );
        let checked = check_program(&program, None, &[]).unwrap();
        let digest = approval_digest(&base, &checked, &cwd);
        let mut other = base.clone();
        other.wake_from.push(WakeFrom {
            workspace: "default".into(),
            project: "intruder".into(),
        });
        assert_ne!(digest, approval_digest(&other, &checked, &cwd));
        let mut other = base.clone();
        other.adapter = WakeAdapter::CodexQueue {
            conversation: ai_memory_core::message_wake::ConversationRef::parse("other").unwrap(),
        };
        assert_ne!(digest, approval_digest(&other, &checked, &cwd));
    }

    #[cfg(unix)]
    #[test]
    fn turning_wake_unverified_on_or_off_voids_the_approval() {
        let tmp = tempfile::tempdir().unwrap();
        let program = script(tmp.path(), "codex", "exit 0", 0o755);
        let cwd = tmp.path().to_path_buf();
        let checked = check_program(&program, None, &[]).unwrap();
        let off = entry(&program);
        let mut on = off.clone();
        on.wake_unverified = true;
        let off_digest = approval_digest(&off, &checked, &cwd);
        let on_digest = approval_digest(&on, &checked, &cwd);
        assert_ne!(off_digest, on_digest, "turning it on needs a new approval");
        let config = Config::default();
        let path = tmp.path().join("approvals.json");
        let mut out = Vec::new();
        assert!(
            approve_interactive(
                &config,
                &on,
                &checked,
                &cwd,
                &path,
                &mut "y\n".as_bytes(),
                &mut out,
            )
            .unwrap()
        );
        let approvals: Approvals = load_json(&config, &path);
        assert!(approvals.approves(&on.name, &on_digest));
        assert!(
            !approvals.approves(&off.name, &off_digest),
            "turning it off again needs a new approval"
        );
        // Off is the digest an entry without the key always had: the extra
        // part is appended only when the option is on.
        let mut legacy = off.clone();
        legacy.wake_unverified = false;
        assert_eq!(off_digest, approval_digest(&legacy, &checked, &cwd));
    }

    /// The digest of an entry that does not set `wake_unverified` is the one
    /// the first supervisor recorded: pinned for the shape of the entry in
    /// use (an agent woken by its own project listed in `wake_from`).
    #[test]
    fn an_entry_without_wake_unverified_keeps_its_recorded_digest() {
        let entry = Entry {
            name: "codex-linha".into(),
            origin: Origin::Configured,
            agent: AgentKind::Codex,
            workspace: "default".into(),
            project: "LinhaDeSucataUnity".into(),
            include_shared: false,
            wake_from: vec![WakeFrom {
                workspace: "default".into(),
                project: "LinhaDeSucataUnity".into(),
            }],
            wake_unverified: false,
            adapter: WakeAdapter::CodexQueue {
                conversation: ai_memory_core::message_wake::ConversationRef::parse(
                    "01a1d9f4-c02a-7380-abcc-ad938cc031d2",
                )
                .unwrap(),
            },
            program: PathBuf::from("/opt/codex/bin/codex"),
            session: None,
            timeout: Duration::from_secs(60),
        };
        let program = CheckedProgram {
            path: PathBuf::from("/opt/codex/bin/codex"),
            configured_dir: PathBuf::from("/opt/codex/bin"),
            sha256: "0".repeat(64),
        };
        let cwd = Path::new("/srv/supervisor/state");
        assert_eq!(
            approval_digest(&entry, &program, cwd),
            "bcf6e633fab17aaa7a47ee554f534b67a0c10d239bdad121502499bf0e362bd2"
        );
        let mut on = entry.clone();
        on.wake_unverified = true;
        assert_ne!(
            approval_digest(&on, &program, cwd),
            approval_digest(&entry, &program, cwd)
        );
    }

    /// A label from a newer server is read as `Unknown` and never wakes,
    /// so the rest of the answer still counts.
    #[test]
    fn an_unknown_sender_label_never_wakes() {
        let reply: CandidatesReply = serde_json::from_str(
            r#"{"messages":[
                {"id":"0190f0a4-5c1e-7a2b-9c3d-4e5f60718291","sender":"from_the_future"},
                {"id":"0190f0a4-5c1e-7a2b-9c3d-4e5f60718292","sender":"same_project"}
            ],"unresolved_wake_from":0}"#,
        )
        .unwrap();
        assert_eq!(reply.messages[0].sender, CandidateSender::Unknown);
        let fresh: Vec<String> = reply.messages.iter().map(|m| m.id.clone()).collect();
        assert_eq!(
            wakeable(&entry(Path::new("/x")), &reply.messages, &fresh),
            ["0190f0a4-5c1e-7a2b-9c3d-4e5f60718292"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_approval_prompt_shows_wake_unverified_and_its_risk() {
        let tmp = tempfile::tempdir().unwrap();
        let program = script(tmp.path(), "codex", "exit 0", 0o755);
        let checked = check_program(&program, None, &[]).unwrap();
        let path = tmp.path().join("approvals.json");
        let shown = |entry: &Entry, language| {
            let config = Config {
                i18n: ai_memory_core::i18n::Localizer::new(language),
                ..Config::default()
            };
            let mut out = Vec::new();
            approve_interactive(
                &config,
                entry,
                &checked,
                tmp.path(),
                &path,
                &mut "n\n".as_bytes(),
                &mut out,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };
        use ai_memory_core::i18n::Language;
        let off = entry(&program);
        let mut on = off.clone();
        on.wake_unverified = true;
        let text = shown(&off, Language::English);
        assert!(text.contains("  wake_unverified: false ("), "{text}");
        assert!(
            text.contains(
                "  own project: other agents of default/linha wake it with no wake_from entry or \
                 recorded session\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  any local process with the server token can wake this agent through mail sent \
                 as its own project; the wake text is fixed and the wake limits still apply\n"
            ),
            "{text}"
        );
        let text = shown(&on, Language::English);
        assert!(
            text.contains(
                "  wake_unverified: true (a listed project wakes it without a recorded sender \
                 session)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  any local process with the server token can wake this agent through mail sent \
                 as its own project or a listed one; the wake text is fixed and the wake limits \
                 still apply\n"
            ),
            "{text}"
        );
        let text = shown(&on, Language::PortugueseBrazil);
        assert!(
            text.contains(
                "  qualquer processo local com o token do servidor pode acordar este agente com \
                 mensagens enviadas como o próprio projeto ou um projeto listado; o texto do \
                 acordar é fixo e os limites de acionamentos continuam valendo\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  wake_unverified: true (um projeto listado o acorda"),
            "{text}"
        );
        let text = shown(&off, Language::PortugueseBrazil);
        assert!(
            text.contains("  wake_unverified: false (um projeto listado"),
            "{text}"
        );
        assert!(
            text.contains("  próprio projeto: outros agentes de default/linha o acordam"),
            "{text}"
        );
        assert!(
            text.contains(
                "pode acordar este agente com mensagens enviadas como o próprio projeto; o texto"
            ),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn approval_needs_an_explicit_yes_and_is_recorded_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let program = script(tmp.path(), "codex", "exit 0", 0o755);
        let checked = check_program(&program, None, &[]).unwrap();
        let config = Config::default();
        let e = entry(&program);
        let path = tmp.path().join("approvals.json");
        for declined in ["\n", "no\n", "", "yes please\n"] {
            let mut out = Vec::new();
            let approved = approve_interactive(
                &config,
                &e,
                &checked,
                tmp.path(),
                &path,
                &mut declined.as_bytes(),
                &mut out,
            )
            .unwrap();
            assert!(!approved, "{declined:?}");
            assert!(!path.exists());
        }
        let mut out = Vec::new();
        assert!(
            approve_interactive(
                &config,
                &e,
                &checked,
                tmp.path(),
                &path,
                &mut "y\n".as_bytes(),
                &mut out,
            )
            .unwrap()
        );
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("--thread"), "{shown}");
        assert!(shown.contains(&checked.sha256), "{shown}");
        assert!(shown.contains("[y/N]"), "{shown}");
        let approvals: Approvals = load_json(&config, &path);
        assert!(approvals.approves(&e.name, &approval_digest(&e, &checked, tmp.path())));
        assert!(!approvals.approves("someone-else", &approval_digest(&e, &checked, tmp.path())));
    }

    #[test]
    fn the_child_environment_is_an_allowlist() {
        let program = CheckedProgram {
            path: PathBuf::from("/opt/codex/lib/codex.js"),
            configured_dir: PathBuf::from("/home/u/.nvm/bin"),
            sha256: String::new(),
        };
        let env = RuntimeEnv::with_process_env_for_tests(&[
            ("HOME", "/home/u"),
            ("CODEX_HOME", "/home/u/.codex"),
            ("AI_MEMORY_AUTH_TOKEN", "secret"),
            ("OPENAI_API_KEY", "sk-secret"),
            ("PATH", "/evil/bin"),
            ("LD_PRELOAD", "/evil.so"),
        ]);
        let adapter = entry(Path::new("/x")).adapter;
        let vars: BTreeMap<String, String> = wake_env(&adapter, &program, &env)
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        assert_eq!(
            vars.keys().collect::<Vec<_>>(),
            ["CODEX_HOME", "HOME", "PATH"]
        );
        assert_eq!(
            vars["PATH"],
            "/home/u/.nvm/bin:/opt/codex/lib:/usr/local/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn only_listed_verified_senders_and_live_receipts_wake() {
        let candidate = |id: &str, sender| Candidate {
            id: id.to_owned(),
            sender,
        };
        let ids = [
            "0190f0a4-5c1e-7a2b-9c3d-4e5f60718291",
            "0190f0a4-5c1e-7a2b-9c3d-4e5f60718292",
            "0190f0a4-5c1e-7a2b-9c3d-4e5f60718293",
            "0190f0a4-5c1e-7a2b-9c3d-4e5f60718294",
            "0190f0a4-5c1e-7a2b-9c3d-4e5f60718295",
        ];
        let candidates = vec![
            candidate(ids[0], CandidateSender::Listed),
            candidate(ids[1], CandidateSender::Unverified),
            candidate(ids[2], CandidateSender::Unlisted),
            candidate(ids[3], CandidateSender::System),
            candidate(ids[4], CandidateSender::Listed),
            candidate("not-an-id", CandidateSender::Listed),
        ];
        let fresh: Vec<String> = ids[..4].iter().map(|id| (*id).to_owned()).collect();
        let off = entry(Path::new("/x"));
        assert_eq!(
            wakeable(&off, &candidates, &fresh),
            [ids[0], ids[3]],
            "unverified and unlisted senders never wake; an already woken id is not fresh"
        );
        let mut on = off.clone();
        on.wake_unverified = true;
        assert_eq!(
            wakeable(&on, &candidates, &fresh),
            [ids[0], ids[1], ids[3]],
            "wake_unverified adds unverified senders and nothing else"
        );
    }

    #[test]
    fn wake_unverified_matrix() {
        let id = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718291";
        let fresh = vec![id.to_owned()];
        let base = entry(Path::new("/x"));
        assert!(base.adapter.wakes_live_session());
        for (sender, off, on) in [
            (CandidateSender::SameProject, true, true),
            (CandidateSender::Linked, true, true),
            (CandidateSender::SameAgent, false, false),
            (CandidateSender::Listed, true, true),
            (CandidateSender::Unverified, false, true),
            (CandidateSender::Unlisted, false, false),
            (CandidateSender::System, true, true),
            (CandidateSender::Unknown, false, false),
        ] {
            for (wake_unverified, expected) in [(false, off), (true, on)] {
                let mut e = base.clone();
                e.wake_unverified = wake_unverified;
                let woken = wakeable(
                    &e,
                    &[Candidate {
                        id: id.to_owned(),
                        sender,
                    }],
                    &fresh,
                );
                assert_eq!(
                    !woken.is_empty(),
                    expected,
                    "{sender:?} with wake_unverified = {wake_unverified}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_adapter_runs_without_a_shell_and_is_stopped_on_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let record = tmp.path().join("argv");
        let program = script(
            tmp.path(),
            "codex",
            &format!(
                "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > {}; pwd >> {0}; env >> {0}; exit 0",
                record.display()
            ),
            0o755,
        );
        let checked = check_program(&program, None, &[]).unwrap();
        let adapter = entry(&program).adapter;
        let text = "it's $(touch pwned) `id`; \"quoted\"";
        let env = vec![(OsString::from("HOME"), OsString::from("/home/u"))];
        let outcome = execute(
            &adapter,
            &checked,
            tmp.path(),
            &env,
            text,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(outcome, WakeOutcome::Attempted);
        let recorded = std::fs::read_to_string(&record).unwrap();
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(&lines[..5], ["queue", "--thread", ID, "--message", text]);
        assert_eq!(
            lines[5],
            std::fs::canonicalize(tmp.path()).unwrap().to_str().unwrap()
        );
        assert!(!tmp.path().join("pwned").exists(), "no shell read the text");
        assert!(lines.contains(&"HOME=/home/u"));
        assert!(!recorded.contains("AI_MEMORY"), "{recorded}");

        let slow = script(tmp.path(), "slow", "sleep 30 & sleep 30", 0o755);
        let checked = check_program(&slow, None, &[]).unwrap();
        let started = Instant::now();
        let outcome = execute(
            &adapter,
            &checked,
            tmp.path(),
            &env,
            "x",
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(outcome, WakeOutcome::Failed(DeliveryReason::AdapterTimeout));
        assert!(started.elapsed() < Duration::from_secs(10));

        let quota = script(
            tmp.path(),
            "quota",
            "echo '{\"error\":{\"type\":\"usage_limit_reached\",\"resets_in_seconds\":120}}' >&2; exit 1",
            0o755,
        );
        let checked = check_program(&quota, None, &[]).unwrap();
        assert_eq!(
            execute(
                &adapter,
                &checked,
                tmp.path(),
                &env,
                "x",
                Duration::from_secs(10)
            )
            .await,
            WakeOutcome::QuotaExhausted {
                retry_after_secs: Some(120)
            }
        );
    }
}
