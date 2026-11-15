//! End-to-end test of `ai-memory message supervise` against a real
//! `ai-memory serve` and a fake `codex` that records how it was run
//! (`docs/agent-messaging.md`, "The wake supervisor").
//!
//! Phase one (budget of two wakes an hour):
//! 1. A message from a listed project with a recorded sender session wakes
//!    the agent once, through the fixed argv, with no sender text.
//! 2. Running again does not wake again for the same message.
//! 3. Mail from an unlisted project, or from the listed one without a
//!    recorded session, wakes nobody.
//! 4. Mail the agent popped before the supervisor looked wakes nobody.
//! 5. The second wake spends the hourly budget; the next message is reported
//!    `wake_failed/budget_exhausted` and its sender gets a receipt.
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
        fn supervise_once(&self, state: &Path) {
            self.cli(&[
                "message",
                "supervise",
                "--once",
                "--state-dir",
                state.to_str().unwrap(),
            ]);
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
                    json!({
                        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "memory_message_send", "arguments": {
                            "from_workspace": WORKSPACE, "from_project": from,
                            "to_workspace": WORKSPACE, "to_project": INBOX,
                            "to_agent": "codex", "subject": "SUBJECT-CANARY", "body": body,
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
    async fn the_supervisor_wakes_once_within_budget_and_reports_every_refusal() {
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

        // 5. The second wake spends the budget; the third is refused.
        let second = f.send(SENDER, Some(SENDER_SESSION), "second").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 2);
        let third = f.send(SENDER, Some(SENDER_SESSION), "third").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 2, "the hourly budget was ignored");
        let report = f.status(&third);
        assert_eq!(report["delivery"]["state"], "wake_failed", "{report}");
        assert_eq!(report["delivery"]["reason"], "budget_exhausted", "{report}");
        let receipts = f.receipts();
        assert!(
            receipts
                .iter()
                .any(|r| r["receipt"]["receipt_for"] == third.as_str()
                    && r["receipt"]["outcome"] == "wake_failed"
                    && r["receipt"]["reason"] == "budget_exhausted"),
            "{receipts:?}"
        );
        for id in [&first, &second, &third] {
            f.pop(id);
        }

        // 6. Phase two: a changed binary is no longer approved.
        let state = f.dd().join("supervisor-2");
        f.approve(&state, false);
        let script = std::fs::read_to_string(f.program()).unwrap();
        std::fs::write(f.program(), format!("{script}# changed\n")).unwrap();
        let fourth = f.send(SENDER, Some(SENDER_SESSION), "fourth").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 2, "a changed binary ran without a new approval");
        let report = f.status(&fourth);
        assert_eq!(report["delivery"]["reason"], "not_approved", "{report}");

        // 7. Approved again, the adapter runs and reports exhausted quota.
        f.approve(&state, false);
        f.set_mode("quota");
        f.supervise_once(&state);
        assert_eq!(f.calls(), 3);
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
        assert_eq!(f.calls(), 3, "the quota circuit did not hold");
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
            3,
            "an unverified sender woke with the option off"
        );
        assert_eq!(f.status(&unverified)["delivery"]["state"], "queued");

        // 9. Turning the option on needs a new approval.
        f.write_config(true);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 3, "wake_unverified ran without a new approval");
        let report = f.status(&unverified);
        assert_eq!(report["delivery"]["reason"], "not_approved", "{report}");

        // 10. Approved with the option on, the unverified message wakes once.
        f.approve(&state, true);
        f.supervise_once(&state);
        assert_eq!(f.calls(), 4);
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
        assert_eq!(f.calls(), 4, "the same message woke twice");

        // 11. An unlisted sender never wakes, even with the option on.
        let unlisted = f.send(OTHER, Some(OTHER_SESSION), "unlisted again").await;
        f.supervise_once(&state);
        assert_eq!(f.calls(), 4, "an unlisted sender woke the agent");
        f.pop(&unlisted);
        f.pop(&unverified);
    }
}
