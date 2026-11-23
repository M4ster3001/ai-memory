//! End-to-end test of `ai-memory message supervise` against a real
//! `ai-memory serve` and a fake `codex` that records how it was run
//! (`docs/agent-messaging.md`, "The wake supervisor").
//!
//! Phase one (the deprecated `wakes_per_hour = 2` is ignored):
//! 1. A message from a listed project with a recorded sender session wakes
//!    the agent once, through the fixed argv, with no sender text.
//! 2. Running again does not wake again for the same message.
//! 3. Mail from an unlisted project, or from the listed one without a
//!    recorded session, wakes nobody.
//! 4. Mail the agent popped before the supervisor looked wakes nobody.
//! 5. New mail that arrives while the woken mail is unread joins it instead
//!    of waking again; once it is read, the next mail wakes at once, and a
//!    third wake within the hour runs too (no hourly budget).
//!
//! Phase two (a fresh state directory):
//! 6. A changed binary voids the approval: `wake_failed/not_approved`.
//! 7. After a new approval, a quota failure is reported as `quota_exhausted`
//!    with `retry_after`, on the message and its receipt, and the open quota
//!    circuit reports the next message the same way without running the
//!    adapter.
//!
//! Phase three (a fresh state directory):
//! 8. Mail from the listed project without a recorded session stays queued
//!    while `wake_unverified` is off.
//! 9. Turning `wake_unverified` on voids the approval: `not_approved`.
//! 10. Approved with the option on, that message wakes the agent once.
//! 11. An unlisted sender still wakes nobody.
//!
//! Phase four (a fresh state directory, `wake_unverified` off again):
//! 12. The agent's own mail to itself wakes nobody.
//! 13. Another agent of the watched project wakes it once, with no
//!     `wake_from` entry for that project and no recorded session, while
//!     unverified mail from the listed project sent at the same time stays
//!     out of the wake.
//!
//! The approval is written the way `message supervise approve` records it,
//! because that command insists on a terminal.

#[cfg(unix)]
mod slow {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};

    use crate::e2e_support::{ServerGuard, hermetic, run_cli, session_count, start_serve};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "supervise-e2e-ws";
    const SENDER: &str = "supervise-e2e-sender";
    const OTHER: &str = "supervise-e2e-other";
    const INBOX: &str = "supervise-e2e-inbox";
    const THREAD: &str = "019a7c2e-0b1d-7f00-9c1e-3a5b6d7e8f90";
    const NAME: &str = "codex-inbox";
    const SENDER_SESSION: &str = "00000000-0000-4000-8000-000000000301";
    const OTHER_SESSION: &str = "00000000-0000-4000-8000-000000000302";

    struct Fixture {
        data_dir: tempfile::TempDir,
        home: tempfile::TempDir,
        bin: tempfile::TempDir,
        base: String,
        _server: ServerGuard,
        client: reqwest::Client,
    }

    impl Fixture {
        fn dd(&self) -> &Path {
            self.data_dir.path()
        }
        fn record(&self) -> PathBuf {
            self.bin.path().join("record")
        }
        fn program(&self) -> PathBuf {
            self.bin.path().join("codex")
        }
        fn calls(&self) -> usize {
            std::fs::read_to_string(self.record())
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "CALL")
                .count()
        }
        fn cli(&self, args: &[&str]) -> String {
            run_cli(args, self.dd(), self.home.path(), None, &self.base)
        }
        /// One supervisor pass. `HOME` and `PATH` point at the fixture, so
        /// a program trusted on first use can only be one of its fakes.
        fn supervise_once(&self, state: &Path) {
            let path = format!("{}:/usr/bin:/bin", self.bin.path().display());
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
        fn supervise_log(state: &Path) -> String {
            std::fs::read_to_string(state.with_extension("log")).unwrap_or_default()
        }
        fn status(&self, id: &str) -> Value {
            serde_json::from_str(&self.cli(&[
                "message",
                "status",
                "--workspace",
                WORKSPACE,
                "--project",
                SENDER,
                "--id",
                id,
                "--json",
            ]))
            .unwrap()
        }
        fn receipts(&self) -> Vec<Value> {
            let out = self.cli(&[
                "message",
                "list",
                "--workspace",
                WORKSPACE,
                "--project",
                SENDER,
                "--json",
            ]);
            serde_json::from_str::<Vec<Value>>(&out)
                .unwrap()
                .into_iter()
                .filter(|m| m["kind"] == "receipt")
                .collect()
        }
        fn pop(&self, id: &str) {
            self.cli(&[
                "message",
                "pop",
                "--workspace",
                WORKSPACE,
                "--project",
                INBOX,
                "--agent",
                "codex",
                "--id",
                id,
                "--json",
            ]);
        }

        /// Send to the codex in the inbox. `session` is the sender's native
        /// hook session id, forwarded as the session-aware MCP bridge does.
        async fn send(&self, from: &str, session: Option<&str>, body: &str) -> String {
            self.send_as(from, session, None, body).await
        }

        /// [`Self::send`] with a declared `from_agent`, as a static MCP
        /// client names its harness.
        async fn send_as(
            &self,
            from: &str,
            session: Option<&str>,
            from_agent: Option<&str>,
            body: &str,
        ) -> String {
            let mut request = self
                .client
                .post(format!("{}/mcp", self.base))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream");
            if let Some(session) = session {
                request = request.header("x-memory-actor-session-id", session);
            }
            let text = request
                .body({
                    let mut arguments = json!({
                        "from_workspace": WORKSPACE, "from_project": from,
                        "to_workspace": WORKSPACE, "to_project": INBOX,
                        "to_agent": "codex", "subject": "SUBJECT-CANARY", "body": body,
                    });
                    if let Some(agent) = from_agent {
                        arguments["from_agent"] = json!(agent);
                    }
                    json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "memory_message_send", "arguments": arguments},
                    })
                    .to_string()
                })
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
            let sent: Value = serde_json::from_str(inner).unwrap();
            sent["message_id"].as_str().unwrap().to_owned()
        }

        /// Record an approval exactly as `message supervise approve` does,
        /// for the entry with `wake_unverified` set as given.
        fn approve(&self, state: &Path, wake_unverified: bool) {
            std::fs::create_dir_all(state).unwrap();
            std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700)).unwrap();
            let cwd = std::fs::canonicalize(state).unwrap();
            let program = std::fs::canonicalize(self.program()).unwrap();
            let sha256 = hex(&Sha256::digest(std::fs::read(&program).unwrap()));
            let mut parts = vec![
                "ai-memory.supervisor-approval/v1".to_owned(),
                NAME.to_owned(),
                "codex".to_owned(),
                WORKSPACE.to_owned(),
                INBOX.to_owned(),
                "false".to_owned(),
                format!("{WORKSPACE}\u{0}{SENDER}"),
                "codex_queue".to_owned(),
                ["queue", "--thread", THREAD, "--message", "{wake}"].join("\u{0}"),
                program.to_str().unwrap().to_owned(),
                sha256.clone(),
                cwd.to_str().unwrap().to_owned(),
                "60".to_owned(),
            ];
            if wake_unverified {
                parts.push("wake_unverified".to_owned());
            }
            let mut hasher = Sha256::new();
            for part in &parts {
                hasher.update((part.len() as u64).to_le_bytes());
                hasher.update(part.as_bytes());
            }
            let approvals = json!({"entries": {NAME: {
                "digest": hex(&hasher.finalize()),
                "program": program,
                "sha256": sha256,
                "approved_at": "2026-11-14T00:00:00Z",
            }}});
            std::fs::write(cwd.join("approvals.json"), approvals.to_string()).unwrap();
        }

        fn set_mode(&self, mode: &str) {
            std::fs::write(self.bin.path().join("mode"), mode).unwrap();
        }

        fn write_config(&self, wake_unverified: bool) {
            write_config(self.dd(), self.bin.path(), wake_unverified);
        }

        /// The argv of the newest adapter call.
        fn last_args(&self) -> Vec<String> {
            let record = std::fs::read_to_string(self.record()).unwrap();
            let last = record.rsplit("CALL\n").next().unwrap_or_default();
            last.lines()
                .filter_map(|line| line.strip_prefix("ARG "))
                .map(str::to_owned)
                .collect()
        }
    }

    fn write_config(data_dir: &Path, bin: &Path, wake_unverified: bool) {
        std::fs::write(
            data_dir.join("config.toml"),
            format!(
                "[messaging.supervisor]\n\
                 max_wakes_per_hour = 20\n\n\
                 [[messaging.supervisor.agents]]\n\
                 name = \"{NAME}\"\n\
                 agent = \"codex\"\n\
                 workspace = \"{WORKSPACE}\"\n\
                 project = \"{INBOX}\"\n\
                 wake_from = [{{ workspace = \"{WORKSPACE}\", project = \"{SENDER}\" }}]\n\
                 wake_unverified = {wake_unverified}\n\
                 adapter = {{ kind = \"codex_queue\", program = \"{}\", thread = \"{THREAD}\" }}\n\
                 wakes_per_hour = 2\n",
                bin.join("codex").display()
            ),
        )
        .unwrap();
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A fake `codex` that records each call's argv, working directory and
    /// environment, then succeeds or fails with Codex's usage-limit error.
    fn fake_codex(dir: &Path) {
        let script = format!(
            "#!/bin/sh\n\
             {{ echo CALL; for a in \"$@\"; do printf 'ARG %s\\n' \"$a\"; done; \
             printf 'CWD %s\\n' \"$(pwd)\"; env | sed 's/^/ENV /'; }} >> {record}\n\
             if [ \"$(cat {mode})\" = quota ]; then \
             echo '{{\"error\":{{\"type\":\"usage_limit_reached\",\"resets_in_seconds\":1800}}}}' >&2; \
             exit 1; fi\n\
             if [ \"$(cat {mode})\" = busy ] && [ \"$1\" = exec ]; then \
             echo 'thread already has an active writer' >&2; exit 1; fi\n\
             if [ \"$(cat {mode})\" = byok ] && [ \"$1\" = exec ]; then \
             echo '{{\"type\":\"error\",\"code\":\"codex_byok_restricted_by_api_credit_promo\"}}' >&2; \
             exit 1; fi\n\
             exit 0\n",
            record = dir.join("record").display(),
            mode = dir.join("mode").display(),
        );
        let path = dir.join("codex");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("mode"), "ok").unwrap();
    }

    async fn session(client: &reqwest::Client, base: &str, project: &str, agent: &str, id: &str) {
        let before = session_count(client, base, WORKSPACE, project).await;
        let resp = client
            .post(format!("{base}/hook"))
            .query(&[
                ("event", "session-start"),
                ("agent", agent),
                ("workspace", WORKSPACE),
                ("project", project),
            ])
            .header("content-type", "application/json")
            .body(
                json!({"event": "session-start", "session_id": id,
                       "cwd": format!("/tmp/supervise-e2e/{project}")})
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        let deadline = Instant::now() + Duration::from_secs(10);
        while session_count(client, base, WORKSPACE, project).await <= before {
            assert!(Instant::now() < deadline, "{project} session never landed");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn fixture() -> Fixture {
        fixture_with(&[]).await
    }

    /// [`fixture`] with extra `serve` arguments (the link test serves the
    /// web dashboard, whose buttons are the root user's confirmation).
    async fn fixture_with(extra: &[&str]) -> Fixture {
        let data_dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Not under /tmp: the supervisor refuses programs in temporary
        // directories.
        let bin = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        fake_codex(bin.path());
        write_config(data_dir.path(), bin.path(), false);
        let client = reqwest::Client::new();
        let (server, base) = start_serve(&client, &data_dir.path().join("serve.log"), |port| {
            let mut cmd = hermetic(BIN);
            cmd.args([
                "serve",
                "--transport",
                "http",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--workspace",
                WORKSPACE,
                "--project",
                SENDER,
                "--no-watcher",
            ])
            .args(extra)
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            .env("AI_MEMORY_HOME", home.path())
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            cmd
        })
        .await;
        session(&client, &base, SENDER, "claude-code", SENDER_SESSION).await;
        session(&client, &base, OTHER, "claude-code", OTHER_SESSION).await;
        session(
            &client,
            &base,
            INBOX,
            "codex",
            "00000000-0000-4000-8000-000000000303",
        )
        .await;
        Fixture {
            data_dir,
            home,
            bin,
            base,
            _server: server,
            client,
        }
    }

    #[tokio::test]
    async fn the_supervisor_wakes_once_coalesces_and_reports_every_refusal() {
        let f = fixture().await;
        let state = f.dd().join("supervisor-1");
        f.approve(&state, false);

        // 1. One wake, fixed argv, no sender text, minimal environment.
        let first = f
            .send(SENDER, Some(SENDER_SESSION), "BODY-CANARY one")
            .await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1);
        let record = std::fs::read_to_string(f.record()).unwrap();
        let args: Vec<&str> = record
            .lines()
            .filter_map(|line| line.strip_prefix("ARG "))
            .collect();
        assert_eq!(args[..4], ["queue", "--thread", THREAD, "--message"]);
        assert_eq!(args.len(), 5, "{args:?}");
        assert!(args[4].contains(&first), "{}", args[4]);
        assert!(
            !record.contains("CANARY"),
            "sender text reached the adapter"
        );
        assert!(
            !record.contains(SENDER),
            "the sender's name reached the adapter"
        );
        assert!(!record.contains("ENV AI_MEMORY"), "{record}");
        let canonical_state = std::fs::canonicalize(&state).unwrap();
        assert!(
            record.contains(&format!("CWD {}", canonical_state.display())),
            "{record}"
        );
        let report = f.status(&first);
        assert_eq!(report["delivery"]["state"], "wake_attempted", "{report}");

        // 2. Same message, no second wake.
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1);

        // 3. Unlisted sender, and the listed project without a session.
        let unlisted = f.send(OTHER, Some(OTHER_SESSION), "unlisted").await;
        let unverified = f.send(SENDER, None, "unverified").await;
        f.supervise_once(&state);
        assert_eq!(
            f.calls(),
            1,
            "an unlisted or unverified sender woke the agent"
        );
        // The unlisted message sits in another sender's outbox; the unverified
        // one shows it was left queued.
        assert_eq!(f.status(&unverified)["delivery"]["state"], "queued");
        f.pop(&unlisted);
        f.pop(&unverified);

        // 4. Collected by hand before the supervisor looked.
        let collected = f.send(SENDER, Some(SENDER_SESSION), "collected").await;
        f.pop(&collected);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1);

        // 5. Mail that arrives while the woken mail is unread joins it.
        let second = f.send(SENDER, Some(SENDER_SESSION), "second").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "a burst woke the agent twice");
        assert_eq!(f.status(&second)["delivery"]["state"], "queued");
        // Once the agent read what it was woken for, new mail wakes at once,
        // and wakes are not counted per hour.
        f.pop(&first);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 2);
        f.pop(&second);
        let third = f.send(SENDER, Some(SENDER_SESSION), "third").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 3, "a third wake in the hour was refused");
        assert_eq!(f.status(&third)["delivery"]["state"], "wake_attempted");
        f.pop(&third);

        // 6. Phase two: a changed binary is no longer approved.
        let state = f.dd().join("supervisor-2");
        f.approve(&state, false);
        let script = std::fs::read_to_string(f.program()).unwrap();
        std::fs::write(f.program(), format!("{script}# changed\n")).unwrap();
        let fourth = f.send(SENDER, Some(SENDER_SESSION), "fourth").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 3, "a changed binary ran without a new approval");
        let report = f.status(&fourth);
        assert_eq!(report["delivery"]["reason"], "not_approved", "{report}");

        // 7. Approved again, the adapter runs and reports exhausted quota.
        f.approve(&state, false);
        f.set_mode("quota");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 4);
        let report = f.status(&fourth);
        assert_eq!(report["delivery"]["state"], "quota_exhausted", "{report}");
        assert_eq!(report["delivery"]["reason"], "quota_marker", "{report}");
        assert!(report["delivery"]["retry_after"].is_string(), "{report}");
        let receipts = f.receipts();
        let quota = receipts
            .iter()
            .find(|r| {
                r["receipt"]["receipt_for"] == fourth.as_str()
                    && r["receipt"]["outcome"] == "quota_exhausted"
            })
            .unwrap_or_else(|| panic!("no quota receipt: {receipts:?}"));
        assert!(quota["receipt"]["retry_after"].is_string(), "{quota}");
        assert!(
            !quota.to_string().contains("usage_limit"),
            "adapter output leaked"
        );

        let fifth = f.send(SENDER, Some(SENDER_SESSION), "fifth").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 4, "the quota circuit did not hold");
        let report = f.status(&fifth);
        assert_eq!(report["delivery"]["state"], "quota_exhausted", "{report}");
        assert!(report["delivery"]["retry_after"].is_string(), "{report}");
        f.pop(&fourth);
        f.pop(&fifth);

        // 8. Phase three: unverified mail stays queued while the option is off.
        let state = f.dd().join("supervisor-3");
        f.set_mode("ok");
        f.approve(&state, false);
        let unverified = f.send(SENDER, None, "unverified again").await;
        f.supervise_once(&state);
        assert_eq!(
            f.calls(),
            4,
            "an unverified sender woke with the option off"
        );
        assert_eq!(f.status(&unverified)["delivery"]["state"], "queued");

        // 9. Turning the option on needs a new approval.
        f.write_config(true);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 4, "wake_unverified ran without a new approval");
        let report = f.status(&unverified);
        assert_eq!(report["delivery"]["reason"], "not_approved", "{report}");

        // 10. Approved with the option on, the unverified message wakes once.
        f.approve(&state, true);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 5);
        let args = f.last_args();
        assert_eq!(args.len(), 5, "{args:?}");
        assert!(args[4].contains(&unverified), "{}", args[4]);
        assert!(
            !args[4].contains("unverified again"),
            "sender text reached the adapter"
        );
        let report = f.status(&unverified);
        assert_eq!(report["delivery"]["state"], "wake_attempted", "{report}");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 5, "the same message woke twice");

        // 11. An unlisted sender never wakes, even with the option on.
        let unlisted = f.send(OTHER, Some(OTHER_SESSION), "unlisted again").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 5, "an unlisted sender woke the agent");
        f.pop(&unlisted);
        f.pop(&unverified);

        // 12. Phase four: the agent's own mail to itself wakes nobody.
        let state = f.dd().join("supervisor-4");
        f.write_config(false);
        f.approve(&state, false);
        let itself = f.send_as(INBOX, None, Some("codex"), "to myself").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 5, "the agent's mail to itself woke it");

        // 13. Another agent of the project wakes it, unverified mail from the
        //     listed project does not join the wake.
        let outside = f.send(SENDER, None, "listed, no session").await;
        let colleague = f
            .send_as(INBOX, None, Some("claude-code"), "same project")
            .await;
        f.supervise_once(&state);
        assert_eq!(
            f.calls(),
            6,
            "same-project mail did not wake the agent once"
        );
        let args = f.last_args();
        assert_eq!(args.len(), 5, "{args:?}");
        assert!(args[4].contains(&colleague), "{}", args[4]);
        assert!(!args[4].contains(&outside), "{}", args[4]);
        assert!(!args[4].contains(&itself), "{}", args[4]);
        assert!(
            !args[4].contains("same project"),
            "sender text reached the adapter"
        );
        let colleague_status: Value = serde_json::from_str(&f.cli(&[
            "message",
            "status",
            "--workspace",
            WORKSPACE,
            "--project",
            INBOX,
            "--id",
            &colleague,
            "--json",
        ]))
        .unwrap();
        assert_eq!(
            colleague_status["delivery"]["state"], "wake_attempted",
            "{colleague_status}"
        );
        assert_eq!(f.status(&outside)["delivery"]["state"], "queued");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 6, "the same message woke twice");
        for id in [&itself, &outside, &colleague] {
            f.pop(id);
        }
    }

    /// The status of mail sent from inside the inbox project.
    fn inbox_status(f: &Fixture, id: &str) -> Value {
        serde_json::from_str(&f.cli(&[
            "message",
            "status",
            "--workspace",
            WORKSPACE,
            "--project",
            INBOX,
            "--id",
            id,
            "--json",
        ]))
        .unwrap()
    }

    /// Record a program approval for discovered agents exactly as
    /// `message supervise approve --adapter codex_queue` does.
    fn approve_program(f: &Fixture, state: &Path) {
        std::fs::create_dir_all(state).unwrap();
        std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cwd = std::fs::canonicalize(state).unwrap();
        let program = std::fs::canonicalize(f.program()).unwrap();
        let sha256 = hex(&Sha256::digest(std::fs::read(&program).unwrap()));
        let parts = [
            "ai-memory.supervisor-program-approval/v1".to_owned(),
            "codex_queue".to_owned(),
            ["queue", "--thread", "{handle}", "--message", "{wake}"].join("\u{0}"),
            program.to_str().unwrap().to_owned(),
            sha256.clone(),
            cwd.to_str().unwrap().to_owned(),
            "60".to_owned(),
        ];
        let mut hasher = Sha256::new();
        for part in &parts {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        let approvals = json!({"programs": {"codex_queue": {
            "digest": hex(&hasher.finalize()),
            "program": program,
            "sha256": sha256,
            "approved_at": "2026-11-15T00:00:00Z",
        }}});
        std::fs::write(cwd.join("approvals.json"), approvals.to_string()).unwrap();
    }

    /// The agent bus with no configured entry: the Codex session the hook
    /// reported in the inbox project is woken through its own thread id by
    /// mail from another agent of that project, and by nothing else.
    #[tokio::test]
    async fn the_bus_wakes_a_discovered_session_from_its_own_project_only() {
        const DISCOVERED_THREAD: &str = "00000000-0000-4000-8000-000000000303";
        let f = fixture().await;
        // The explicit approval path: no trust on first use.
        std::fs::write(
            f.dd().join("config.toml"),
            "[messaging.supervisor]\nmax_wakes_per_hour = 20\ntrust_on_first_use = false\n",
        )
        .unwrap();
        let state = f.dd().join("supervisor-bus");

        // 1. No program approved for the adapter: reported, not run.
        let first = f
            .send_as(INBOX, None, Some("claude-code"), "BODY-CANARY bus")
            .await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 0, "an unapproved program ran");
        let status = inbox_status(&f, &first);
        assert_eq!(status["delivery"]["state"], "wake_failed", "{status}");
        assert_eq!(status["delivery"]["reason"], "not_approved", "{status}");

        // 2. Approved once for the adapter: the session's own thread id is
        //    the target, and the text carries no sender data.
        approve_program(&f, &state);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1);
        let args = f.last_args();
        assert_eq!(
            args[..4],
            ["queue", "--thread", DISCOVERED_THREAD, "--message"]
        );
        assert_eq!(args.len(), 5, "{args:?}");
        assert!(args[4].contains(&first), "{}", args[4]);
        assert!(
            !args[4].contains("CANARY"),
            "sender text reached the adapter"
        );
        assert_eq!(
            inbox_status(&f, &first)["delivery"]["state"],
            "wake_attempted"
        );
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "the same message woke twice");

        // 3. Mail from another project, with a recorded session or not, and
        //    the agent's mail to itself wake nobody: no link, no wake_from.
        let outside = f.send(SENDER, Some(SENDER_SESSION), "outside").await;
        let itself = f.send_as(INBOX, None, Some("codex"), "to myself").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "mail from outside the project woke the agent");
        assert_eq!(f.status(&outside)["delivery"]["state"], "queued");

        // 4. A changed binary voids the program approval. The woken mail is
        //    read first, so the next one is not coalesced into it.
        pop_as(&f, "codex", &first);
        let colleague = f.send_as(INBOX, None, Some("claude-code"), "second").await;
        let script = std::fs::read_to_string(f.program()).unwrap();
        std::fs::write(f.program(), format!("{script}\n# upgraded\n")).unwrap();
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "a changed program ran on an old approval");
        assert_eq!(
            inbox_status(&f, &colleague)["delivery"]["reason"],
            "not_approved"
        );
        for id in [&outside, &itself, &colleague] {
            f.pop(id);
        }
    }

    /// Rooms through the CLI against the real server: a post with an
    /// `@codex` mention wakes the discovered Codex session through the bus,
    /// with the fixed wake text, and codex reads the post from the room.
    #[tokio::test]
    async fn a_room_mention_wakes_the_mentioned_agent_through_the_bus() {
        const DISCOVERED_THREAD: &str = "00000000-0000-4000-8000-000000000303";
        let f = fixture().await;
        std::fs::write(
            f.dd().join("config.toml"),
            "[messaging.supervisor]\nmax_wakes_per_hour = 20\n",
        )
        .unwrap();
        let state = f.dd().join("supervisor-room");
        approve_program(&f, &state);
        let scope = ["--workspace", WORKSPACE, "--project", INBOX];

        // A plain post wakes nobody.
        let plain: Value = serde_json::from_str(
            &f.cli(
                &[
                    &[
                        "room",
                        "post",
                        "general",
                        "BODY-CANARY plain",
                        "--agent",
                        "claude-code",
                    ][..],
                    &scope[..],
                ]
                .concat(),
            ),
        )
        .unwrap();
        assert_eq!(plain["seq"], 1, "{plain}");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 0, "a post without a mention woke an agent");

        // A mention does, once, through the session's own thread.
        let asked: Value = serde_json::from_str(
            &f.cli(
                &[
                    &[
                        "room",
                        "post",
                        "general",
                        "BODY-CANARY @codex need a hand",
                        "--agent",
                        "claude-code",
                    ][..],
                    &scope[..],
                ]
                .concat(),
            ),
        )
        .unwrap();
        let mention = asked["mentioned"][0]["message_id"]
            .as_str()
            .unwrap()
            .to_owned();
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1);
        let args = f.last_args();
        assert_eq!(
            args[..4],
            ["queue", "--thread", DISCOVERED_THREAD, "--message"]
        );
        assert!(args[4].contains(&mention), "{}", args[4]);
        assert!(!args[4].contains("CANARY"), "post text reached the adapter");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "the mention woke twice");

        // Codex reads the room as a non-member from the start of the history.
        let read: Value = serde_json::from_str(
            &f.cli(
                &[
                    &[
                        "room", "read", "general", "--agent", "codex", "--after", "0",
                    ][..],
                    &scope[..],
                ]
                .concat(),
            ),
        )
        .unwrap();
        assert_eq!(read["posts"].as_array().unwrap().len(), 2, "{read}");
        assert_eq!(read["posts"][1]["body"], "BODY-CANARY @codex need a hand");
        let listed: Value = serde_json::from_str(
            &f.cli(&[&["room", "list", "--agent", "claude-code"][..], &scope[..]].concat()),
        )
        .unwrap();
        assert_eq!(
            listed["rooms"][0]["members"],
            json!(["claude-code"]),
            "{listed}"
        );
        assert_eq!(listed["rooms"][0]["unread"], 0);
        f.pop(&mention);
    }

    const CLAUDE_SESSION: &str = "00000000-0000-4000-8000-000000000304";
    const CODEX_SESSION: &str = "00000000-0000-4000-8000-000000000303";
    const STOPPED_THREAD: &str = "00000000-0000-4000-8000-000000000305";

    /// Post one hook event for a session of `agent` in `project`.
    async fn hook(f: &Fixture, project: &str, agent: &str, body: Value) {
        let event = body["event"].as_str().unwrap().to_owned();
        let resp = f
            .client
            .post(format!("{}/hook", f.base))
            .query(&[
                ("event", event.as_str()),
                ("agent", agent),
                ("workspace", WORKSPACE),
                ("project", project),
            ])
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
    }

    /// Mail inside the inbox project from one harness to another.
    async fn send_to(f: &Fixture, from_agent: &str, to_agent: &str, body: &str) -> String {
        let text = f
            .client
            .post(format!("{}/mcp", f.base))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "memory_message_send", "arguments": {
                        "from_workspace": WORKSPACE, "from_project": INBOX,
                        "from_agent": from_agent,
                        "to_workspace": WORKSPACE, "to_project": INBOX,
                        "to_agent": to_agent, "body": body,
                    }},
                })
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
        let sent: Value = serde_json::from_str(inner).unwrap();
        sent["message_id"].as_str().unwrap().to_owned()
    }

    /// Wait until the server shows `agent`'s newest session in the inbox
    /// project as open or ended (hook events are ingested asynchronously).
    async fn wait_for_session(f: &Fixture, agent: &str, open: bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status: Value = f
                .client
                .get(format!("{}/inbox/bus/status", f.base))
                .header("x-ai-memory-client", "e2e")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let seen = status["agents"].as_array().is_some_and(|agents| {
                agents.iter().any(|row| {
                    row["project"] == INBOX && row["agent"] == agent && row["open"] == open
                })
            });
            if seen {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{agent}: never open={open}: {status}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Run supervisor passes until `done` holds: hook events are ingested
    /// asynchronously, so the session a test just ended may still read open
    /// for a moment.
    async fn supervise_until(f: &Fixture, state: &Path, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            f.supervise_once(state);
            if done() {
                return;
            }
            assert!(Instant::now() < deadline, "the supervisor never got there");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn pop_as(f: &Fixture, agent: &str, id: &str) {
        f.cli(&[
            "message",
            "pop",
            "--workspace",
            WORKSPACE,
            "--project",
            INBOX,
            "--agent",
            agent,
            "--id",
            id,
            "--json",
        ]);
    }

    fn approvals(state: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(state.join("approvals.json")).unwrap())
            .unwrap()
    }

    /// The default path: the supervisor trusts the `codex` it finds the
    /// first time, records path and hash, and trusts a changed binary again
    /// with a log line instead of asking.
    #[tokio::test]
    async fn a_known_adapter_is_trusted_on_first_use_and_a_changed_binary_is_logged() {
        let f = fixture().await;
        std::fs::write(f.dd().join("config.toml"), "[messaging.supervisor]\n").unwrap();
        let state = f.dd().join("supervisor-tofu");
        let first = send_to(&f, "claude-code", "codex", "first").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1);
        let trusted = &approvals(&state)["programs"]["codex_queue"];
        assert_eq!(trusted["trusted"], "first_use", "{trusted}");
        let program = std::fs::canonicalize(f.program()).unwrap();
        assert_eq!(trusted["program"], program.to_str().unwrap());
        assert!(Fixture::supervise_log(&state).contains("trusting on first use"));
        pop_as(&f, "codex", &first);

        let script = std::fs::read_to_string(f.program()).unwrap();
        std::fs::write(f.program(), format!("{script}\n# upgraded\n")).unwrap();
        let second = send_to(&f, "claude-code", "codex", "second").await;
        f.supervise_once(&state);
        assert_eq!(
            f.calls(),
            2,
            "a changed binary of a known adapter was refused"
        );
        let trusted = &approvals(&state)["programs"]["codex_queue"];
        assert_eq!(trusted["trusted"], "changed", "{trusted}");
        assert!(trusted["previous_sha256"].is_string(), "{trusted}");
        assert!(Fixture::supervise_log(&state).contains("the program changed"));
        pop_as(&f, "codex", &second);
    }

    /// A Codex conversation nobody has open is resumed headless. When the
    /// Codex login refuses headless turns, the supervisor reports nothing
    /// lost: it queues the turn for when the conversation is opened, and does
    /// not retry the refused adapter.
    #[tokio::test]
    async fn a_stopped_codex_conversation_is_resumed_and_a_refused_login_falls_back_to_the_queue() {
        let f = fixture().await;
        std::fs::write(f.dd().join("config.toml"), "[messaging.supervisor]\n").unwrap();
        // A Codex conversation that ran in a real project directory, then
        // ended; the fixture's own session ends too.
        let project = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        for (event, id) in [
            ("session-start", STOPPED_THREAD),
            ("session-end", STOPPED_THREAD),
            ("session-end", CODEX_SESSION),
        ] {
            hook(
                &f,
                INBOX,
                "codex",
                json!({"event": event, "session_id": id, "cwd": project.path()}),
            )
            .await;
            // The server acknowledges a hook before storing it; keep the
            // start ahead of the end, as a real session does.
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        wait_for_session(&f, "codex", false).await;
        let state = f.dd().join("supervisor-exec");
        f.set_mode("byok");
        let first = send_to(&f, "claude-code", "codex", "first").await;
        supervise_until(&f, &state, || f.calls() >= 2).await;
        assert_eq!(f.calls(), 2, "exec resume, then the queue");
        let record = std::fs::read_to_string(f.record()).unwrap();
        let exec: Vec<&str> = record
            .split("CALL\n")
            .nth(1)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("ARG "))
            .collect();
        assert_eq!(
            exec[..6],
            [
                "exec",
                "resume",
                "--skip-git-repo-check",
                "-c",
                "sandbox_mode=\"read-only\"",
                STOPPED_THREAD
            ]
        );
        assert!(exec[6].contains(&first), "{exec:?}");
        let cwd = std::fs::canonicalize(project.path()).unwrap();
        assert!(
            record.contains(&format!("CWD {}", cwd.display())),
            "{record}"
        );
        assert_eq!(f.last_args()[..3], ["queue", "--thread", STOPPED_THREAD]);
        assert_eq!(
            inbox_status(&f, &first)["delivery"]["state"],
            "wake_attempted"
        );
        assert!(Fixture::supervise_log(&state).contains("auth_failed; trying codex_queue"));
        pop_as(&f, "codex", &first);

        // The refused adapter is not run again while its block lasts.
        let second = send_to(&f, "claude-code", "codex", "second").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 3);
        assert_eq!(f.last_args()[0], "queue");
        pop_as(&f, "codex", &second);

        // With a login that allows it, the resume itself runs.
        f.set_mode("ok");
        let state = f.dd().join("supervisor-exec-ok");
        let third = send_to(&f, "claude-code", "codex", "third").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 4);
        assert_eq!(f.last_args()[..2], ["exec", "resume"]);
        pop_as(&f, "codex", &third);
    }

    /// Distinct mail texts: the server's loop guard pauses a pair that
    /// repeats one body.
    const BODIES: [&str; 5] = [
        "the build finished on the first runner",
        "which review comment should land before the release",
        "a flaky test needs another look today",
        "please confirm the migration plan for the archive",
        "lunch is ready whenever you are",
    ];

    /// How many wake prompts reached the Codex queue (the adapter calls
    /// whose first argument is `queue`).
    fn queue_calls(f: &Fixture) -> usize {
        std::fs::read_to_string(f.record())
            .unwrap_or_default()
            .split("CALL\n")
            .skip(1)
            .filter(|call| call.lines().next() == Some("ARG queue"))
            .count()
    }

    /// A stopped Codex conversation whose headless resume finds the thread
    /// busy (`codex_exec_resume` is `session_busy`, the turn is queued).
    async fn busy_codex(f: &Fixture) {
        std::fs::write(f.dd().join("config.toml"), "[messaging.supervisor]\n").unwrap();
        let project = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        for (event, id) in [
            ("session-start", STOPPED_THREAD),
            ("session-end", STOPPED_THREAD),
            ("session-end", CODEX_SESSION),
        ] {
            hook(
                f,
                INBOX,
                "codex",
                json!({"event": event, "session_id": id, "cwd": project.path()}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        wait_for_session(f, "codex", false).await;
        f.set_mode("busy");
        // The project directory must outlive the test's supervisor passes.
        std::mem::forget(project);
    }

    /// A Codex turn that runs a long command is busy: the headless resume
    /// is refused and each wake lands in the app's follow-up queue. Mail
    /// the agent already read mid-turn (through the tool-call notice) does
    /// not make the queued prompt go away, so a later message must not add a
    /// second prompt behind it.
    #[tokio::test]
    async fn a_busy_codex_gets_one_queued_wake_even_when_it_reads_mail_mid_turn() {
        let f = fixture().await;
        busy_codex(&f).await;
        let state = f.dd().join("supervisor-busy-pile");
        let mut ids = Vec::new();
        for n in 1..=5 {
            let id = send_to(&f, "claude-code", "codex", BODIES[n - 1]).await;
            f.supervise_once(&state);
            ids.push(id.clone());
            // Read in the middle of the long turn.
            pop_as(&f, "codex", &id);
        }
        assert_eq!(queue_calls(&f), 1, "{}", Fixture::supervise_log(&state));
        let record = std::fs::read_to_string(f.record()).unwrap();
        assert!(!record.contains("runner"), "mail text reached the adapter");
        for id in &ids {
            assert_eq!(inbox_status(&f, id)["delivery"]["state"], "delivered");
        }
    }

    /// Five messages arrive while the thread is busy: one prompt waits in
    /// the queue. Once the agent has read what the prompt announced, the
    /// mail that arrived meanwhile is announced by the next wake, with its
    /// own count, and nothing is lost.
    #[tokio::test]
    async fn mail_that_arrives_behind_a_queued_wake_is_announced_once_it_is_read() {
        let f = fixture().await;
        busy_codex(&f).await;
        let state = f.dd().join("supervisor-busy-burst");
        let mut ids = Vec::new();
        for n in 1..=5 {
            ids.push(send_to(&f, "claude-code", "codex", BODIES[n - 1]).await);
            f.supervise_once(&state);
        }
        assert_eq!(queue_calls(&f), 1, "{}", Fixture::supervise_log(&state));
        assert_eq!(
            inbox_status(&f, &ids[0])["delivery"]["state"],
            "wake_attempted"
        );
        for id in &ids[1..] {
            assert_eq!(
                inbox_status(&f, id)["delivery"]["state"],
                "queued",
                "{}",
                Fixture::supervise_log(&state)
            );
        }
        // The thread goes idle and the queued prompt ran: the agent read
        // the first message only.
        f.set_mode("ok");
        pop_as(&f, "codex", &ids[0]);
        supervise_until(&f, &state, || f.calls() >= 3).await;
        let args = f.last_args();
        assert_eq!(args[..2], ["exec", "resume"], "{args:?}");
        assert!(
            args.iter().any(|a| a.contains("4 new message(s)")),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a.contains(&ids[4])), "{args:?}");
        for id in &ids[1..] {
            assert_eq!(inbox_status(&f, id)["delivery"]["state"], "wake_attempted");
            pop_as(&f, "codex", id);
        }
    }

    /// A fake `claude`: `agents --json` lists the session as live in mode
    /// `live`, and `-p` prints a successful JSON result.
    fn fake_claude(dir: &Path) {
        let script = format!(
            "#!/bin/sh\n\
             {{ echo CALL; for a in \"$@\"; do printf 'ARG %s\\n' \"$a\"; done; \
             printf 'CWD %s\\n' \"$(pwd)\"; env | sed 's/^/ENV /'; }} >> {record}\n\
             if [ \"$1\" = agents ]; then \
             if [ \"$(cat {mode})\" = live ]; then \
             echo '[{{\"sessionId\":\"{CLAUDE_SESSION}\",\"pid\":4242,\"status\":\"idle\"}}]'; \
             else echo '[]'; fi; exit 0; fi\n\
             echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}}'\n",
            record = dir.join("claude-record").display(),
            mode = dir.join("claude-mode").display(),
        );
        let path = dir.join("claude");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("claude-mode"), "ok").unwrap();
    }

    fn claude_calls(f: &Fixture) -> Vec<Vec<String>> {
        std::fs::read_to_string(f.bin.path().join("claude-record"))
            .unwrap_or_default()
            .split("CALL\n")
            .skip(1)
            .map(|call| call.lines().map(str::to_owned).collect())
            .collect()
    }

    /// A stopped Claude Code session is resumed with the fixed text, the
    /// ai-memory tools only and in its own directory, after
    /// `claude agents --json` shows no process holding it; a live one and a
    /// directory anyone can write are never resumed.
    #[tokio::test]
    async fn a_stopped_claude_session_is_resumed_in_its_directory_only_when_no_process_holds_it() {
        let f = fixture().await;
        std::fs::write(f.dd().join("config.toml"), "[messaging.supervisor]\n").unwrap();
        fake_claude(f.bin.path());
        let project = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        hook(
            &f,
            INBOX,
            "claude-code",
            json!({"event": "session-start", "session_id": CLAUDE_SESSION,
                   "cwd": project.path()}),
        )
        .await;
        // The server acknowledges a hook before storing it; keep the start
        // ahead of the end, as a real session does.
        tokio::time::sleep(Duration::from_millis(400)).await;
        hook(
            &f,
            INBOX,
            "claude-code",
            json!({"event": "session-end", "session_id": CLAUDE_SESSION,
                   "cwd": project.path()}),
        )
        .await;
        wait_for_session(&f, "claude-code", false).await;
        let state = f.dd().join("supervisor-claude");

        // A process still holds the session: nothing is resumed.
        std::fs::write(f.bin.path().join("claude-mode"), "live").unwrap();
        let first = send_to(&f, "codex", "claude-code", "BODY-CANARY claude").await;
        supervise_until(&f, &state, || !claude_calls(&f).is_empty()).await;
        let calls = claude_calls(&f);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].iter().any(|line| line == "ARG agents"));
        assert_eq!(inbox_status(&f, &first)["delivery"]["state"], "queued");

        // Stopped: resumed in its directory with the fixed text.
        std::fs::write(f.bin.path().join("claude-mode"), "ok").unwrap();
        f.supervise_once(&state);
        let calls = claude_calls(&f);
        assert_eq!(calls.len(), 3, "probe, then the resume: {calls:?}");
        let args: Vec<&str> = calls[2]
            .iter()
            .filter_map(|line| line.strip_prefix("ARG "))
            .collect();
        assert_eq!(args[0], "-p");
        assert!(args[1].contains(&first), "{}", args[1]);
        assert!(
            !calls[2].concat().contains("CANARY"),
            "sender text reached claude"
        );
        assert_eq!(args[2..4], ["--resume", CLAUDE_SESSION]);
        assert_eq!(args[args.len() - 2..], ["--allowedTools", "mcp__ai-memory"]);
        let cwd = std::fs::canonicalize(project.path()).unwrap();
        assert!(
            calls[2].contains(&format!("CWD {}", cwd.display())),
            "{:?}",
            calls[2]
        );
        assert!(
            !calls[2]
                .iter()
                .any(|line| line.starts_with("ENV AI_MEMORY"))
        );
        assert!(
            !calls[2]
                .iter()
                .any(|line| line.starts_with("ENV CLAUDE_CODE_"))
        );
        assert_eq!(
            inbox_status(&f, &first)["delivery"]["state"],
            "wake_attempted"
        );
        pop_as(&f, "claude-code", &first);

        // A directory every user can write is not trusted to run a resume.
        std::fs::set_permissions(project.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let second = send_to(&f, "codex", "claude-code", "second").await;
        f.supervise_once(&state);
        assert_eq!(claude_calls(&f).len(), 4, "only the probe ran");
        let status = inbox_status(&f, &second);
        assert_eq!(
            status["delivery"]["reason"], "session_not_found",
            "{status}"
        );
        std::fs::set_permissions(project.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        pop_as(&f, "claude-code", &second);
    }

    /// The loop guard through the real server and supervisor: three copies
    /// of one body pause the pair, the mail stays readable but wakes nobody
    /// and is reported `loop_suspected`, and a human prompt resumes it.
    #[tokio::test]
    async fn a_pair_that_repeats_itself_is_paused_until_a_human_prompt() {
        let f = fixture().await;
        std::fs::write(f.dd().join("config.toml"), "[messaging.supervisor]\n").unwrap();
        let state = f.dd().join("supervisor-loop");
        let mut ids = Vec::new();
        for n in 1..=3 {
            ids.push(
                send_to(
                    &f,
                    "claude-code",
                    "codex",
                    &format!("Ping {n}: still waiting"),
                )
                .await,
            );
        }
        f.supervise_once(&state);
        assert_eq!(f.calls(), 0, "a paused pair woke the agent");
        for id in &ids {
            let status = inbox_status(&f, id);
            assert_eq!(status["delivery"]["reason"], "loop_suspected", "{status}");
        }
        assert!(Fixture::supervise_log(&state).contains("loop suspected"));

        hook(
            &f,
            INBOX,
            "codex",
            json!({"event": "user-prompt-submit", "session_id": CODEX_SESSION,
                   "prompt": "carry on with the review"}),
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            f.supervise_once(&state);
            if f.calls() == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the human prompt did not resume the pair"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        for id in &ids {
            pop_as(&f, "codex", id);
        }
    }

    /// The Claude Code `Stop` hook: `message watch --rewake` blocks until
    /// mail from another agent of the project arrives, then exits 2 with
    /// the fixed wake text on stderr; mail it already announced, or the
    /// agent's own mail, does not wake the session again.
    #[tokio::test]
    async fn the_rewake_hook_exits_2_once_for_new_mail_from_another_agent() {
        use std::io::Write as _;
        let f = fixture().await;
        hook(
            &f,
            INBOX,
            "claude-code",
            json!({"event": "session-start", "session_id": CLAUDE_SESSION}),
        )
        .await;
        wait_for_session(&f, "claude-code", true).await;
        let spawn = || {
            let mut child = hermetic(BIN)
                .args([
                    "message",
                    "watch",
                    "--rewake",
                    "--agent",
                    "claude-code",
                    "--workspace",
                    WORKSPACE,
                    "--project",
                    INBOX,
                ])
                .env("AI_MEMORY_DATA_DIR", f.dd())
                .env("AI_MEMORY_HOME", f.home.path())
                .env("AI_MEMORY_SERVER_URL", &f.base)
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
                .env("RUST_LOG", "off")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            write!(
                stdin,
                r#"{{"session_id":"{CLAUDE_SESSION}","hook_event_name":"Stop","stop_hook_active":false}}"#
            )
            .unwrap();
            drop(stdin);
            child
        };
        let wait = |child: &mut std::process::Child, secs: u64| {
            let deadline = Instant::now() + Duration::from_secs(secs);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    return Some(status);
                }
                if Instant::now() > deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        let mut hook_process = spawn();
        std::thread::sleep(Duration::from_millis(1_500));
        assert!(
            hook_process.try_wait().unwrap().is_none(),
            "it exited with no mail"
        );
        // The agent's own mail does not wake it.
        let own = send_to(&f, "claude-code", "claude-code", "a note to myself").await;
        assert!(wait(&mut hook_process, 2).is_none(), "its own mail woke it");
        let id = send_to(&f, "codex", "claude-code", "BODY-CANARY review done").await;
        let status = wait(&mut hook_process, 15).expect("new mail did not wake the session");
        assert_eq!(status.code(), Some(2));
        let out = hook_process.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.starts_with("ai-memory: 1 new message(s) for claude-code"),
            "{stderr}"
        );
        assert!(
            stderr.contains(&id) && !stderr.contains("CANARY"),
            "{stderr}"
        );
        assert!(out.stdout.is_empty());
        assert_eq!(inbox_status(&f, &id)["delivery"]["state"], "wake_attempted");

        // The next turn's hook does not announce the same mail again.
        let mut again = spawn();
        assert!(
            wait(&mut again, 3).is_none(),
            "announced mail woke the session twice"
        );
        let _ = again.kill();
        let _ = again.wait();
        pop_as(&f, "claude-code", &id);
        pop_as(&f, "claude-code", &own);
    }

    /// Run the native `ai-memory hook` the way Codex runs it around a tool
    /// call, from `dir`, against the fixture's server. Returns its stdout and
    /// how long it took.
    fn tool_hook(
        f: &Fixture,
        dir: &Path,
        server: &str,
        event: &str,
        session: &str,
    ) -> (String, Duration) {
        use std::io::Write as _;
        let started = Instant::now();
        let mut child = hermetic(BIN)
            .args([
                "hook",
                "--agent",
                "codex",
                "--event",
                event,
                "--server-url",
                server,
            ])
            .env("AI_MEMORY_DATA_DIR", f.dd().join("hook-data"))
            .env("AI_MEMORY_HOME", f.home.path())
            .env("HOME", f.home.path())
            .env("RUST_LOG", "off")
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let hook_name = if event == "pre-tool-use" {
            "PreToolUse"
        } else {
            "PostToolUse"
        };
        let body = json!({
            "session_id": session, "cwd": dir, "hook_event_name": hook_name,
            "tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_response": "ok",
        });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        (String::from_utf8(out.stdout).unwrap(), started.elapsed())
    }

    fn context_of(stdout: &str) -> Option<String> {
        stdout.lines().find_map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()?
                .pointer("/hookSpecificOutput/additionalContext")?
                .as_str()
                .map(str::to_owned)
        })
    }

    /// The mid-turn notice: a Codex `PostToolUse` hook adds one fixed line
    /// naming new mail from another agent of the project, once per message
    /// and at most every 15 seconds; never the agent's own mail, never text;
    /// and a server that does not answer costs the tool call nothing.
    #[tokio::test]
    async fn a_busy_codex_hears_of_new_mail_between_tool_calls() {
        let f = fixture().await;
        let project = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        std::fs::write(
            project.path().join(".ai-memory.toml"),
            format!("workspace = \"{WORKSPACE}\"\nproject = \"{INBOX}\"\n"),
        )
        .unwrap();
        const SESSION_A: &str = "00000000-0000-4000-8000-000000000306";
        const SESSION_B: &str = "00000000-0000-4000-8000-000000000307";

        let (out, _) = tool_hook(&f, project.path(), &f.base, "post-tool-use", SESSION_A);
        assert_eq!(context_of(&out), None, "an empty inbox says nothing: {out}");

        let id = send_to(&f, "claude-code", "codex", "BODY-CANARY build is green").await;
        // Within the check interval of the first call: not checked yet.
        let (out, _) = tool_hook(&f, project.path(), &f.base, "pre-tool-use", SESSION_A);
        assert_eq!(context_of(&out), None, "checked again too soon: {out}");
        std::thread::sleep(Duration::from_secs(5));
        let (out, _) = tool_hook(&f, project.path(), &f.base, "pre-tool-use", SESSION_A);
        let notice = context_of(&out).unwrap_or_else(|| panic!("no notice: {out}"));
        assert!(
            notice.starts_with(&format!(
                "ai-memory: 1 new message(s) arrived for codex in {WORKSPACE}/{INBOX}"
            )),
            "{notice}"
        );
        assert!(notice.contains(&id) && !out.contains("CANARY"), "{out}");
        let line: Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(line["hookSpecificOutput"]["hookEventName"], "PreToolUse");

        // The next tool call does not repeat it.
        let (out, _) = tool_hook(&f, project.path(), &f.base, "post-tool-use", SESSION_A);
        assert_eq!(context_of(&out), None, "{out}");

        // Another session that never heard of the mail is told; once read,
        // the agent's own mail to itself never makes a notice.
        pop_as(&f, "codex", &id);
        let own = send_to(&f, "codex", "codex", "a note to myself").await;
        let (out, _) = tool_hook(&f, project.path(), &f.base, "post-tool-use", SESSION_B);
        assert_eq!(context_of(&out), None, "its own mail was announced: {out}");
        pop_as(&f, "codex", &own);

        // A server that accepts the connection and never answers.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let silent_url = format!("http://{}", silent.local_addr().unwrap());
        let (out, took) = tool_hook(
            &f,
            project.path(),
            &silent_url,
            "post-tool-use",
            "00000000-0000-4000-8000-000000000308",
        );
        assert_eq!(context_of(&out), None);
        assert!(took < Duration::from_secs(3), "the hook waited {took:?}");
        drop(silent);
    }

    /// Plug and play in a throwaway home: one `agent-bus install --yes`
    /// writes the native hooks of every agent it finds, the Claude Code wake
    /// hook and the user service, and starts it; a second run changes
    /// nothing. A fake `systemctl` records what it was asked.
    #[tokio::test]
    async fn one_install_wires_every_agent_found_and_is_idempotent() {
        let f = fixture().await;
        let home = tempfile::tempdir().unwrap();
        for dir in [".claude", ".codex", ".gemini"] {
            std::fs::create_dir(home.path().join(dir)).unwrap();
        }
        let tools = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let record = tools.path().join("systemctl.log");
        std::fs::write(
            tools.path().join("systemctl"),
            format!("#!/bin/sh\necho \"$@\" >> {}\n", record.display()),
        )
        .unwrap();
        std::fs::set_permissions(
            tools.path().join("systemctl"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let data = tempfile::tempdir().unwrap();
        let install = || {
            let out = hermetic(BIN)
                .args(["agent-bus", "install", "--yes", "--hooks-dir"])
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../hooks"))
                .env_clear()
                .env("HOME", home.path())
                .env("PATH", format!("{}:/usr/bin:/bin", tools.path().display()))
                .env("AI_MEMORY_DATA_DIR", data.path())
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_SERVER_URL", &f.base)
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
                .env("RUST_LOG", "off")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "install failed: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let read = |path: &str| std::fs::read_to_string(home.path().join(path)).unwrap_or_default();
        let printed = install();
        for agent in ["claude-code", "codex", "gemini-cli"] {
            assert!(
                printed.contains(&format!("Wrote the {agent} hooks")),
                "{printed}"
            );
        }
        let codex = read(".codex/hooks.json");
        assert!(
            codex.contains("hook --event post-tool-use") && codex.contains("--agent codex"),
            "{codex}"
        );
        let claude = read(".claude/settings.json");
        assert!(claude.contains("hook --event post-tool-use"), "{claude}");
        assert!(
            claude.contains("message watch --rewake --agent claude-code"),
            "{claude}"
        );
        assert_eq!(
            claude.matches("message watch --rewake").count(),
            1,
            "{claude}"
        );
        assert!(!read(".gemini/settings.json").is_empty());
        let unit = read(".config/systemd/user/ai-memory-supervisor.service");
        assert!(unit.contains("message supervise"), "{unit}");
        let calls = std::fs::read_to_string(&record).unwrap();
        assert!(
            calls.contains("--user enable --now ai-memory-supervisor.service"),
            "{calls}"
        );

        // Running it again rewrites the same files.
        let before = (
            read(".codex/hooks.json"),
            read(".claude/settings.json"),
            unit,
        );
        install();
        let after = (
            read(".codex/hooks.json"),
            read(".claude/settings.json"),
            read(".config/systemd/user/ai-memory-supervisor.service"),
        );
        assert_eq!(before, after, "a second install changed the files");
    }

    /// Links between projects through the real server, the CLI and the
    /// supervisor: mail from another project waits until a link joins the
    /// two; a request opens nothing; an agent cannot confirm what it was not
    /// shown; the destination's web page confirms; the mail then wakes the
    /// discovered Codex once, and not again after the link expires (the
    /// clock, with no sweep) or is closed. The text never reaches the
    /// adapter.
    #[tokio::test]
    async fn a_link_between_two_projects_wakes_until_it_expires_or_is_closed() {
        let f = fixture_with(&["--enable-web"]).await;
        std::fs::write(f.dd().join("config.toml"), "[messaging.supervisor]\n").unwrap();
        let state = f.dd().join("supervisor-link");

        // 1. No link: delivered, not woken.
        let first = f
            .send(SENDER, Some(SENDER_SESSION), "BODY-CANARY before the link")
            .await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 0, "mail from another project woke the agent");
        assert_eq!(f.status(&first)["delivery"]["state"], "queued");

        // 2. A request, filed by the sender's agent, opens nothing.
        let filed: Value = serde_json::from_str(&f.cli(&[
            "link",
            "request",
            "--workspace",
            WORKSPACE,
            "--project",
            SENDER,
            "--to-workspace",
            WORKSPACE,
            "--to-project",
            INBOX,
            "--reason",
            "review the schema together",
            "--agent",
            "claude-code",
            "--session",
            SENDER_SESSION,
        ]))
        .unwrap();
        assert_eq!(filed["link"]["state"], "requested", "{filed}");
        let id = filed["link"]["id"].as_str().unwrap().to_owned();
        f.supervise_once(&state);
        assert_eq!(f.calls(), 0, "a request woke the agent");

        // 3. The destination's agent, never shown the request, cannot
        //    confirm it; the requester's project never can.
        let confirm = |project: &str, agent: &str, session: &str| {
            let client = f.client.clone();
            let url = format!("{}/admin/links/confirm", f.base);
            let body = json!({"id": id, "workspace": WORKSPACE, "project": project,
                              "agent": agent, "session": session});
            async move {
                let response = client.post(url).json(&body).send().await.unwrap();
                let status = response.status();
                (status, response.json::<Value>().await.unwrap())
            }
        };
        let (status, refused) = confirm(INBOX, "codex", CODEX_SESSION).await;
        assert_eq!(status, 409, "{refused}");
        assert_eq!(refused["code"], "not_presented", "{refused}");
        let (_, refused) = confirm(SENDER, "claude-code", SENDER_SESSION).await;
        assert_eq!(refused["code"], "not_destination", "{refused}");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 0);

        // 4. The root user presses Confirm on the destination's page. The
        //    sender's page is the wrong one.
        let web = |project: &str| {
            let client = f.client.clone();
            let url = format!("{}/web/chat/links/{id}/confirm", f.base);
            let body = json!({"workspace": WORKSPACE, "project": project});
            async move {
                client
                    .post(url)
                    .header("x-ai-memory-ui", "1")
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
            }
        };
        assert_eq!(web(SENDER).await, 409);
        assert_eq!(web(INBOX).await, 200);

        // 5. The waiting mail and a new one wake the discovered Codex once,
        //    through its own thread, with the fixed text.
        let second = f
            .send(SENDER, Some(SENDER_SESSION), "BODY-CANARY during the link")
            .await;
        supervise_until(&f, &state, || f.calls() >= 1).await;
        assert_eq!(f.calls(), 1);
        let args = f.last_args();
        assert_eq!(args[..3], ["queue", "--thread", CODEX_SESSION], "{args:?}");
        assert!(!args[4].contains("CANARY"), "mail text reached the adapter");
        assert_eq!(f.status(&second)["delivery"]["state"], "wake_attempted");
        pop_as(&f, "codex", &first);
        pop_as(&f, "codex", &second);

        // 6. The clock ends the link, with no sweep in between.
        {
            let conn = rusqlite::Connection::open(f.dd().join("db").join("memory.sqlite"))
                .expect("open store");
            conn.busy_timeout(Duration::from_secs(10)).unwrap();
            let uuid = uuid::Uuid::parse_str(&id).unwrap();
            let changed = conn
                .execute(
                    "UPDATE agent_links SET expires_at = 1 WHERE id = ?1",
                    [uuid.as_bytes().to_vec()],
                )
                .expect("move the clock");
            assert_eq!(changed, 1);
        }
        let third = f
            .send(SENDER, Some(SENDER_SESSION), "BODY-CANARY after expiry")
            .await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "an expired link woke the agent");
        assert_eq!(f.status(&third)["delivery"]["state"], "queued");
        pop_as(&f, "codex", &third);

        // 7. A new link, closed by either side, stops waking too.
        let again: Value = serde_json::from_str(&f.cli(&[
            "link",
            "request",
            "--workspace",
            WORKSPACE,
            "--project",
            SENDER,
            "--to-workspace",
            WORKSPACE,
            "--to-project",
            INBOX,
            "--reason",
            "one more round",
            "--agent",
            "claude-code",
            "--session",
            SENDER_SESSION,
        ]))
        .unwrap();
        let second_id = again["link"]["id"].as_str().unwrap().to_owned();
        assert_ne!(second_id, id, "an expired link is not reused");
        let confirmed = f
            .client
            .post(format!("{}/web/chat/links/{second_id}/confirm", f.base))
            .header("x-ai-memory-ui", "1")
            .json(&json!({"workspace": WORKSPACE, "project": INBOX}))
            .send()
            .await
            .unwrap();
        assert_eq!(confirmed.status(), 200);
        let closed: Value = serde_json::from_str(&f.cli(&["link", "close", &second_id])).unwrap();
        assert_eq!(closed["closed"], true, "{closed}");
        let fourth = f
            .send(SENDER, Some(SENDER_SESSION), "BODY-CANARY after close")
            .await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 1, "a closed link woke the agent");
        pop_as(&f, "codex", &fourth);
        let listed: Value = serde_json::from_str(&f.cli(&[
            "link",
            "list",
            "--workspace",
            WORKSPACE,
            "--project",
            INBOX,
            "--all",
        ]))
        .unwrap();
        let states: Vec<&str> = listed["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| link["state"].as_str().unwrap())
            .collect();
        assert!(
            states.contains(&"expired") && states.contains(&"closed"),
            "{states:?}"
        );
    }
}
