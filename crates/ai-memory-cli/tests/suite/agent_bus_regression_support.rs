//! Shared fixture of the agent bus regression suite, end-to-end layer
//! (`docs/agent-bus/TESTING.md`): a real `ai-memory serve` on a free
//! loopback port with a throwaway data directory, a fake `codex` and a fake
//! `claude` that only record how they were run, and helpers to send mail,
//! report hook sessions and run one pass of the real wake supervisor.
//!
//! Nothing here reaches a network, a model, the operator's `~/.claude` or
//! `~/.codex`, or the production server: the home directory, data directory,
//! token (none) and port are all created per test.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::e2e_support::{ServerGuard, hermetic, run_cli, session_count, start_serve};

pub const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
pub const WS: &str = "bus-regression";
/// The project most tests work in: both agents of "inside one project".
pub const API: &str = "bus-api";
/// A second project, for mail between projects.
pub const WEB: &str = "bus-web";
/// A third project nobody links to.
pub const OTHER: &str = "bus-other";

pub const CODEX_API: &str = "00000000-0000-4000-8000-00000000b001";
pub const CLAUDE_API: &str = "00000000-0000-4000-8000-00000000b002";
pub const CODEX_WEB: &str = "00000000-0000-4000-8000-00000000b003";
pub const CLAUDE_WEB: &str = "00000000-0000-4000-8000-00000000b004";

/// A session id for a tool hook: each distinct id has its own notice state,
/// so a test never needs to wait out the five-second check interval.
pub fn tool_session(n: u32) -> String {
    format!("00000000-0000-4000-8000-0000000c{n:04x}")
}

pub struct Bus {
    pub data_dir: TempDir,
    pub home: TempDir,
    /// Not under `/tmp`: the supervisor refuses programs in temporary
    /// directories.
    pub tools: TempDir,
    pub base: String,
    pub client: reqwest::Client,
    web: bool,
    server: Option<ServerGuard>,
}

impl Bus {
    /// A server with `codex` and `claude-code` sessions already running in
    /// [`API`]. With `web`, the dashboard is served too (the root user's
    /// buttons confirm links).
    pub async fn start(web: bool) -> Self {
        let tools = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        fake_codex(tools.path());
        fake_claude(tools.path());
        let data_dir = tempfile::tempdir().unwrap();
        // The default supervisor: no entry, agents discovered from hooks,
        // programs trusted on first use.
        std::fs::write(
            data_dir.path().join("config.toml"),
            "[messaging.supervisor]\n",
        )
        .unwrap();
        let mut bus = Self {
            data_dir,
            home: tempfile::tempdir().unwrap(),
            tools,
            base: String::new(),
            client: reqwest::Client::new(),
            web,
            server: None,
        };
        bus.spawn().await;
        bus.session(API, "codex", CODEX_API).await;
        bus.session(API, "claude-code", CLAUDE_API).await;
        bus
    }

    async fn spawn(&mut self) {
        let (server, base) = start_serve(
            &self.client,
            &self.data_dir.path().join("serve.log"),
            |port| {
                let mut cmd = hermetic(BIN);
                cmd.args([
                    "serve",
                    "--transport",
                    "http",
                    "--bind",
                    &format!("127.0.0.1:{port}"),
                    "--workspace",
                    WS,
                    "--project",
                    API,
                    "--no-watcher",
                ])
                .env("AI_MEMORY_DATA_DIR", self.data_dir.path())
                .env("AI_MEMORY_HOME", self.home.path())
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
                if self.web {
                    cmd.arg("--enable-web");
                }
                cmd
            },
        )
        .await;
        self.server = Some(server);
        self.base = base;
    }

    /// Stop the server and start it again on the same data directory (a new
    /// port, the same files), as a service restart does.
    pub async fn restart(&mut self) {
        drop(self.server.take());
        self.spawn().await;
    }

    pub fn dd(&self) -> &Path {
        self.data_dir.path()
    }

    pub fn state_dir(&self, name: &str) -> PathBuf {
        self.dd().join(name)
    }

    // ---- the fakes -----------------------------------------------------

    pub fn codex_record(&self) -> PathBuf {
        self.tools.path().join("record")
    }

    /// How many times the fake `codex` ran.
    pub fn calls(&self) -> usize {
        std::fs::read_to_string(self.codex_record())
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "CALL")
            .count()
    }

    /// The whole record of the fake `codex`: argv, working directory, env.
    pub fn record(&self) -> String {
        std::fs::read_to_string(self.codex_record()).unwrap_or_default()
    }

    /// The argv of the newest `codex` call.
    pub fn last_args(&self) -> Vec<String> {
        let record = self.record();
        let last = record.rsplit("CALL\n").next().unwrap_or_default();
        last.lines()
            .filter_map(|line| line.strip_prefix("ARG "))
            .map(str::to_owned)
            .collect()
    }

    /// Every call of the fake `claude`, each as its recorded lines.
    pub fn claude_calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.tools.path().join("claude-record"))
            .unwrap_or_default()
            .split("CALL\n")
            .skip(1)
            .map(|call| call.lines().map(str::to_owned).collect())
            .collect()
    }

    /// `ok` (the session is not held) or `live` (a process holds it).
    pub fn set_claude_mode(&self, mode: &str) {
        std::fs::write(self.tools.path().join("claude-mode"), mode).unwrap();
    }

    // ---- the supervisor --------------------------------------------------

    /// One pass of the real wake supervisor. `HOME` and `PATH` point at the
    /// fixture, so a program trusted on first use can only be a fake.
    pub fn supervise_once(&self, state: &Path) {
        let path = format!("{}:/usr/bin:/bin", self.tools.path().display());
        let out = hermetic(BIN)
            .args([
                "message",
                "supervise",
                "--once",
                "--state-dir",
                state.to_str().unwrap(),
            ])
            .env("AI_MEMORY_DATA_DIR", self.dd())
            .env("AI_MEMORY_HOME", self.home.path())
            .env("AI_MEMORY_SERVER_URL", &self.base)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("RUST_LOG", "off")
            .env("HOME", self.home.path())
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "supervise failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::write(state.with_extension("log"), &out.stderr).unwrap();
    }

    /// What the last pass over `state` logged.
    pub fn supervise_log(state: &Path) -> String {
        std::fs::read_to_string(state.with_extension("log")).unwrap_or_default()
    }

    /// Passes until `done`: hook events are ingested asynchronously, so a
    /// session a test just ended may still read open for a moment. Bounded
    /// and short-stepped; it never sleeps when `done` already holds.
    pub async fn supervise_until(&self, state: &Path, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.supervise_once(state);
            if done() {
                return;
            }
            assert!(Instant::now() < deadline, "the supervisor never got there");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ---- the CLI ---------------------------------------------------------

    pub fn cli(&self, args: &[&str]) -> String {
        run_cli(args, self.dd(), self.home.path(), None, &self.base)
    }

    pub fn cli_json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.cli(args)).unwrap()
    }

    /// The delivery report of a message, as its sender sees it.
    pub fn status(&self, sender_project: &str, id: &str) -> Value {
        self.cli_json(&[
            "message",
            "status",
            "--workspace",
            WS,
            "--project",
            sender_project,
            "--id",
            id,
            "--json",
        ])
    }

    /// `(state, reason)` of a message's delivery.
    pub fn delivery(&self, sender_project: &str, id: &str) -> (String, Option<String>) {
        let report = self.status(sender_project, id);
        (
            report["delivery"]["state"].as_str().unwrap().to_owned(),
            report["delivery"]["reason"].as_str().map(str::to_owned),
        )
    }

    /// Pop `id` as `agent` in `project`.
    pub fn pop(&self, project: &str, agent: &str, id: &str) -> Value {
        self.cli_json(&[
            "message",
            "pop",
            "--workspace",
            WS,
            "--project",
            project,
            "--agent",
            agent,
            "--id",
            id,
            "--json",
        ])
    }

    /// The pending mail of a project's inbox that `agent` may read, receipts
    /// included: addressed to it, or shared by the whole project.
    pub fn inbox(&self, project: &str, agent: &str) -> Vec<Value> {
        let listed = self.cli_json(&[
            "message",
            "list",
            "--workspace",
            WS,
            "--project",
            project,
            "--json",
        ]);
        listed
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m["to_agent"].is_null() || m["to_agent"] == agent)
            .collect()
    }

    /// The messages (no receipts) waiting for `agent` in `project`.
    pub fn waiting(&self, project: &str, agent: &str) -> Vec<Value> {
        self.inbox(project, agent)
            .into_iter()
            .filter(|m| m["kind"] == "message" && m["to_agent"] == agent)
            .collect()
    }

    // ---- hooks and mail ----------------------------------------------------

    /// Post one hook event for a session, the way a lifecycle hook does.
    pub async fn hook(&self, project: &str, agent: &str, body: Value) {
        let event = body["event"].as_str().unwrap().to_owned();
        let response = self
            .client
            .post(format!("{}/hook", self.base))
            .query(&[
                ("event", event.as_str()),
                ("agent", agent),
                ("workspace", WS),
                ("project", project),
            ])
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }

    /// Start a session of `agent` in `project` and wait until the server
    /// has recorded it (hook events are stored after they are acknowledged).
    pub async fn session(&self, project: &str, agent: &str, id: &str) {
        let before = session_count(&self.client, &self.base, WS, project).await;
        self.hook(
            project,
            agent,
            json!({"event": "session-start", "session_id": id,
                   "cwd": format!("/tmp/bus-regression/{project}")}),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while session_count(&self.client, &self.base, WS, project).await <= before {
            assert!(Instant::now() < deadline, "{project} session never landed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A human prompt typed into a session.
    pub async fn prompt(&self, project: &str, agent: &str, session: &str, text: &str) {
        self.hook(
            project,
            agent,
            json!({"event": "user-prompt-submit", "session_id": session, "prompt": text}),
        )
        .await;
    }

    /// Wait until the bus status shows `agent`'s newest session in `project`
    /// as open or ended.
    pub async fn wait_for_session(&self, project: &str, agent: &str, open: bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status: Value = self
                .client
                .get(format!("{}/inbox/bus/status", self.base))
                .header("x-ai-memory-client", "e2e")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let seen = status["agents"].as_array().is_some_and(|agents| {
                agents.iter().any(|row| {
                    row["project"] == project && row["agent"] == agent && row["open"] == open
                })
            });
            if seen {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{project}/{agent}: never open={open}: {status}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `memory_message_send` through the real MCP endpoint. `session` is the
    /// sender's native hook session id, as a session-aware bridge forwards
    /// it. Returns the message id.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        from_project: &str,
        from_agent: &str,
        session: Option<&str>,
        to_project: &str,
        to_agent: &str,
        subject: &str,
        body: &str,
        extra: Value,
    ) -> String {
        let mut arguments = json!({
            "from_workspace": WS, "from_project": from_project, "from_agent": from_agent,
            "to_workspace": WS, "to_project": to_project, "to_agent": to_agent,
            "subject": subject, "body": body,
        });
        if let (Some(arguments), Some(extra)) = (arguments.as_object_mut(), extra.as_object()) {
            arguments.extend(extra.clone());
        }
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(session) = session {
            request = request.header("x-memory-actor-session-id", session);
        }
        let text = request
            .body(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": "memory_message_send", "arguments": arguments}})
                .to_string(),
            )
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&text).unwrap();
        let inner = reply
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no tool result: {text}"));
        let sent: Value = serde_json::from_str(inner).unwrap_or_else(|_| panic!("{inner}"));
        sent["message_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no message id in {sent}"))
            .to_owned()
    }

    /// Mail between two agents with plain text.
    pub async fn mail(
        &self,
        from_project: &str,
        from_agent: &str,
        to_project: &str,
        to_agent: &str,
        body: &str,
    ) -> String {
        self.send(
            from_project,
            from_agent,
            None,
            to_project,
            to_agent,
            "a subject",
            body,
            json!({}),
        )
        .await
    }

    // ---- links -------------------------------------------------------------

    /// An agent of `from_project` asks for a link to `to_project`.
    pub fn link_request(&self, from_project: &str, to_project: &str, session: &str) -> Value {
        self.cli_json(&[
            "link",
            "request",
            "--workspace",
            WS,
            "--project",
            from_project,
            "--to-workspace",
            WS,
            "--to-project",
            to_project,
            "--reason",
            "review the schema together",
            "--agent",
            "claude-code",
            "--session",
            session,
        ])
    }

    /// `POST /admin/links/confirm` as an agent: `(status, body)`.
    pub async fn agent_confirm(
        &self,
        link: &str,
        project: &str,
        agent: &str,
        session: &str,
    ) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}/admin/links/confirm", self.base))
            .json(&json!({"id": link, "workspace": WS, "project": project,
                          "agent": agent, "session": session}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }

    /// The root user's Confirm button on `project`'s page: the status.
    pub async fn web_confirm(&self, link: &str, project: &str) -> u16 {
        self.client
            .post(format!("{}/web/chat/links/{link}/confirm", self.base))
            .header("x-ai-memory-ui", "1")
            .json(&json!({"workspace": WS, "project": project}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// The chat page of `project`, as the root user reads it.
    pub async fn chat_page(&self, project: &str) -> String {
        self.client
            .get(format!("{}/web/chat", self.base))
            .query(&[("workspace", WS), ("project", project)])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// Set a link's stored end of life, to move its clock without waiting.
    pub fn move_link_clock(&self, link: &str, expires_at_us: i64) {
        let conn = rusqlite::Connection::open(self.dd().join("db").join("memory.sqlite"))
            .expect("open store");
        conn.busy_timeout(Duration::from_secs(10)).unwrap();
        let uuid = uuid::Uuid::parse_str(link).unwrap();
        let changed = conn
            .execute(
                "UPDATE agent_links SET expires_at = ?2 WHERE id = ?1",
                rusqlite::params![uuid.as_bytes().to_vec(), expires_at_us],
            )
            .expect("move the clock");
        assert_eq!(changed, 1);
    }

    // ---- the mid-turn notice ---------------------------------------------

    /// A project directory the native hook can resolve to `project`.
    pub fn project_dir(&self, project: &str) -> TempDir {
        let dir = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        std::fs::write(
            dir.path().join(".ai-memory.toml"),
            format!("workspace = \"{WS}\"\nproject = \"{project}\"\n"),
        )
        .unwrap();
        dir
    }

    /// Run the native `ai-memory hook` the way Codex runs it around a tool
    /// call, from `dir`. Returns its stdout.
    pub fn tool_hook(&self, dir: &Path, event: &str, session: &str) -> String {
        use std::io::Write as _;
        let mut child = hermetic(BIN)
            .args([
                "hook",
                "--agent",
                "codex",
                "--event",
                event,
                "--server-url",
                &self.base,
            ])
            .env("AI_MEMORY_DATA_DIR", self.dd().join("hook-data"))
            .env("AI_MEMORY_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env("RUST_LOG", "off")
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let name = if event == "pre-tool-use" {
            "PreToolUse"
        } else {
            "PostToolUse"
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id": session, "cwd": dir, "hook_event_name": name,
                       "tool_name": "Bash", "tool_input": {"command": "ls"},
                       "tool_response": "ok"})
                .to_string()
                .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

/// The notice a tool hook's stdout carries, if any.
pub fn notice_of(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        serde_json::from_str::<Value>(line)
            .ok()?
            .pointer("/hookSpecificOutput/additionalContext")?
            .as_str()
            .map(str::to_owned)
    })
}

fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake `codex` that records each call's argv, working directory and
/// environment and succeeds.
fn fake_codex(dir: &Path) {
    write_executable(
        &dir.join("codex"),
        &format!(
            "#!/bin/sh\n\
             {{ echo CALL; for a in \"$@\"; do printf 'ARG %s\\n' \"$a\"; done; \
             printf 'CWD %s\\n' \"$(pwd)\"; env | sed 's/^/ENV /'; }} >> {record}\n\
             exit 0\n",
            record = dir.join("record").display(),
        ),
    );
}

/// A fake `claude`: `agents --json` lists [`CLAUDE_STOPPED`] as live in mode
/// `live`, and `-p` prints a successful JSON result.
fn fake_claude(dir: &Path) {
    write_executable(
        &dir.join("claude"),
        &format!(
            "#!/bin/sh\n\
             {{ echo CALL; for a in \"$@\"; do printf 'ARG %s\\n' \"$a\"; done; \
             printf 'CWD %s\\n' \"$(pwd)\"; env | sed 's/^/ENV /'; }} >> {record}\n\
             if [ \"$1\" = agents ]; then \
             if [ \"$(cat {mode})\" = live ]; then \
             echo '[{{\"sessionId\":\"{CLAUDE_STOPPED}\",\"pid\":4242,\"status\":\"idle\"}}]'; \
             else echo '[]'; fi; exit 0; fi\n\
             echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}}'\n",
            record = dir.join("claude-record").display(),
            mode = dir.join("claude-mode").display(),
        ),
    );
    std::fs::write(dir.join("claude-mode"), "ok").unwrap();
}

/// The Claude Code session the tests stop to see how a stopped one is
/// resumed.
pub const CLAUDE_STOPPED: &str = "00000000-0000-4000-8000-00000000b0a1";
