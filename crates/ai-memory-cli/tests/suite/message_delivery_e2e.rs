//! End-to-end test of message delivery states and receipts against a real
//! `ai-memory serve` (`docs/agent-messaging.md`, "Delivery states and
//! receipts").
//!
//! 1. A send to a harness that never ran in the recipient project is accepted
//!    as `awaiting_agent`, and becomes `queued` when that harness's first
//!    session starts there.
//! 2. A pop marks it `delivered`; `message status` shows the timeline.
//! 3. Expiry: the test moves a message's deadline into the past while the
//!    server is stopped (the minimum time to live is a minute, too long for a
//!    test), restarts the server, and its startup sweep expires the message and
//!    puts a receipt in the sender's inbox. The receipt carries no message
//!    text, the recipient cannot cancel it, and popping it produces no
//!    receipt.

mod slow {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::e2e_support::{ServerGuard, hermetic, run_cli, session_count, start_serve};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "delivery-e2e-ws";
    const SENDER: &str = "delivery-e2e-sender";
    const INBOX: &str = "delivery-e2e-inbox";
    const BODY: &str = "BODY-CANARY please do the thing";
    const SUBJECT: &str = "SUBJECT-CANARY";

    async fn serve(
        client: &reqwest::Client,
        data_dir: &Path,
        home: &Path,
    ) -> (ServerGuard, String) {
        start_serve(client, &data_dir.join("serve.log"), |port| {
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
            .env("AI_MEMORY_DATA_DIR", data_dir)
            .env("AI_MEMORY_HOME", home)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            cmd
        })
        .await
    }

    async fn session(client: &reqwest::Client, base: &str, project: &str, agent: &str, nth: u8) {
        let session_id = format!("00000000-0000-4000-8000-0000000002{nth:02x}");
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
                json!({
                    "event": "session-start",
                    "session_id": session_id,
                    "cwd": format!("/tmp/delivery-e2e/{project}"),
                })
                .to_string(),
            )
            .send()
            .await
            .expect("hook request");
        assert!(resp.status().is_success(), "{}", resp.status());
        let deadline = Instant::now() + Duration::from_secs(10);
        while session_count(client, base, WORKSPACE, project).await <= before {
            assert!(Instant::now() < deadline, "{project} session never landed");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn mcp_send(client: &reqwest::Client, base: &str, arguments: Value) -> Value {
        let text = client
            .post(format!("{base}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": "memory_message_send", "arguments": arguments },
                })
                .to_string(),
            )
            .send()
            .await
            .expect("mcp request")
            .text()
            .await
            .expect("mcp body");
        let reply: Value = serde_json::from_str(&text).expect("JSON-RPC reply");
        let inner = reply
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no tool result: {text}"));
        serde_json::from_str(inner).expect("tool JSON")
    }

    fn status(data_dir: &Path, home: &Path, base: &str, id: &str) -> Value {
        let out = run_cli(
            &[
                "message",
                "status",
                "--workspace",
                WORKSPACE,
                "--project",
                SENDER,
                "--id",
                id,
                "--json",
            ],
            data_dir,
            home,
            None,
            base,
        );
        serde_json::from_str(&out).expect("status --json")
    }

    fn inbox(data_dir: &Path, home: &Path, base: &str, project: &str) -> Vec<Value> {
        let out = run_cli(
            &[
                "message",
                "list",
                "--workspace",
                WORKSPACE,
                "--project",
                project,
                "--json",
            ],
            data_dir,
            home,
            None,
            base,
        );
        serde_json::from_str(&out).expect("list --json")
    }

    #[tokio::test]
    async fn delivery_states_and_receipts_round_trip_through_a_real_server() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let (dd, hd) = (data_dir.path(), home.path());
        let client = reqwest::Client::new();
        let (server, base) = serve(&client, dd, hd).await;
        session(&client, &base, SENDER, "claude-code", 1).await;
        session(&client, &base, INBOX, "claude-code", 2).await;

        // 1. Unregistered harness: accepted and awaiting.
        let sent = mcp_send(
            &client,
            &base,
            json!({
                "from_workspace": WORKSPACE, "from_project": SENDER,
                "to_workspace": WORKSPACE, "to_project": INBOX,
                "to_agent": "codex", "subject": SUBJECT, "body": BODY,
                "notify": ["success", "failure"],
            }),
        )
        .await;
        assert_eq!(sent["delivery"]["state"], "awaiting_agent", "{sent}");
        let first = sent["message_id"].as_str().unwrap().to_owned();

        session(&client, &base, INBOX, "codex", 3).await;
        let report = status(dd, hd, &base, &first);
        assert_eq!(report["delivery"]["state"], "queued", "{report}");
        assert_eq!(report["events"][0]["event"], "agent_registered", "{report}");

        // 2. Pop: delivered, and the `success` receipt reaches the sender.
        run_cli(
            &[
                "message",
                "pop",
                "--workspace",
                WORKSPACE,
                "--project",
                INBOX,
                "--agent",
                "codex",
                "--id",
                &first,
                "--json",
            ],
            dd,
            hd,
            None,
            &base,
        );
        let report = status(dd, hd, &base, &first);
        assert_eq!(report["delivery"]["state"], "delivered", "{report}");
        let receipts = inbox(dd, hd, &base, SENDER);
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_eq!(receipts[0]["receipt"]["outcome"], "delivered");

        // 3. Expiry through the server's own sweep.
        let sent = mcp_send(
            &client,
            &base,
            json!({
                "from_workspace": WORKSPACE, "from_project": SENDER,
                "to_workspace": WORKSPACE, "to_project": INBOX,
                "subject": SUBJECT, "body": BODY, "ttl_seconds": 60,
            }),
        )
        .await;
        let second = sent["message_id"].as_str().unwrap().to_owned();
        drop(server);
        {
            let conn = rusqlite::Connection::open(dd.join("db").join("memory.sqlite"))
                .expect("open store");
            let id = uuid::Uuid::parse_str(&second).unwrap();
            let changed = conn
                .execute(
                    "UPDATE agent_messages SET expires_at = 1 WHERE id = ?1",
                    [id.as_bytes().to_vec()],
                )
                .expect("move the deadline");
            assert_eq!(changed, 1);
        }
        let (_server, base) = serve(&client, dd, hd).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let report = status(dd, hd, &base, &second);
            if report["delivery"]["state"] == "expired" {
                assert_eq!(report["delivery"]["reason"], "ttl");
                assert_eq!(report["state"], "expired");
                break;
            }
            assert!(Instant::now() < deadline, "never expired: {report}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let receipts = inbox(dd, hd, &base, SENDER);
        let expired: Vec<&Value> = receipts
            .iter()
            .filter(|r| r["receipt"]["outcome"] == "expired")
            .collect();
        assert_eq!(expired.len(), 1, "{receipts:?}");
        let receipt = expired[0];
        assert_eq!(receipt["kind"], "receipt");
        assert_eq!(receipt["receipt"]["receipt_for"], second.as_str());
        let text = receipt.to_string();
        assert!(
            !text.contains("CANARY"),
            "receipt leaked message text: {text}"
        );

        // The recipient cannot retract the receipt about it.
        let out = run_cli(
            &[
                "message",
                "cancel",
                "--workspace",
                WORKSPACE,
                "--project",
                INBOX,
                "--all",
                "--json",
            ],
            dd,
            hd,
            None,
            &base,
        );
        let cancelled: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(cancelled["cancelled"], 0);

        // Popping every receipt produces no further receipt anywhere.
        for r in &receipts {
            let id = r["id"].as_str().unwrap();
            run_cli(
                &[
                    "message",
                    "pop",
                    "--workspace",
                    WORKSPACE,
                    "--project",
                    SENDER,
                    "--agent",
                    "claude-code",
                    "--id",
                    id,
                    "--json",
                ],
                dd,
                hd,
                None,
                &base,
            );
        }
        assert!(inbox(dd, hd, &base, SENDER).is_empty());
        assert!(inbox(dd, hd, &base, INBOX).is_empty());
    }
}
