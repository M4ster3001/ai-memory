//! The agent bus through the real hook router: hook ingest records a wake
//! handle only for a harness that has one and a canonical UUID session id,
//! and `GET /inbox/bus` lists the agents with addressed mail and a handle,
//! with ids only, for the root credential only.

use std::time::Duration;

use ai_memory_core::{
    AgentKind, AuthLevel, AuthorizedViewer, MessageNature, NewAgentMessage, UserId,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::inbox_watch::{Harness, INBOX, WORKSPACE, call, harness};

const CODEX_THREAD: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718293";
const NEWER_THREAD: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718294";
const CLAUDE_SESSION: &str = "5f1d2c3b-4a59-4687-9786-a5b4c3d2e1f0";

fn bus_request(query: &str, marker: bool) -> Request<Body> {
    let mut builder = Request::get(format!("/inbox/bus?{query}"));
    if marker {
        builder = builder.header(crate::CLIENT_MARKER_HEADER, "cli");
    }
    builder.body(Body::empty()).unwrap()
}

async fn bus(h: &Harness, query: &str) -> (StatusCode, Value) {
    let (status, body) = call(&h.app, bus_request(query, true)).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(Value::String(body)),
    )
}

/// POST a hook event for `session` the way a lifecycle hook does.
async fn hook(h: &Harness, event: &str, agent: &str, session: &str) {
    let request = Request::post(format!(
        "/hook?event={event}&agent={agent}&workspace={WORKSPACE}&project={INBOX}"
    ))
    .header("content-type", "application/json")
    .body(Body::from(
        json!({"event": event, "session_id": session, "cwd": "/tmp/agent-bus"}).to_string(),
    ))
    .unwrap();
    let status = h.app.clone().oneshot(request).await.unwrap().status();
    assert!(status.is_success(), "{event} for {agent}: {status}");
}

async fn handles(h: &Harness) -> Vec<(AgentKind, ai_memory_store::WakeHandle)> {
    h.state
        .reader
        .wake_handles(h.inbox.0, h.inbox.1)
        .await
        .unwrap()
}

/// Hook ingest is fire-and-forget: poll until `count` handles exist.
async fn until_handles(h: &Harness, count: usize) -> Vec<(AgentKind, ai_memory_store::WakeHandle)> {
    for _ in 0..250 {
        let found = handles(h).await;
        if found.len() >= count {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handles(h).await
}

async fn send_to(h: &Harness, to_agent: Option<AgentKind>) -> String {
    h.state
        .writer
        .insert_message(NewAgentMessage {
            from_workspace_id: h.inbox.0,
            from_project_id: h.inbox.1,
            from_agent: AgentKind::ClaudeCode,
            from_session_id: None,
            from_owner_user: None,
            to_workspace_id: h.inbox.0,
            to_project_id: h.inbox.1,
            to_agent,
            subject: Some("SUBJECT-CANARY".into()),
            body: "BODY-CANARY".into(),
            nature: MessageNature::Unknown,
            evidence: None,
            priority: ai_memory_core::MessagePriority::Normal,
            notify: ai_memory_core::NotifyOn::DEFAULT,
            ttl_secs: None,
        })
        .await
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn hooks_record_a_handle_only_for_a_wakeable_harness_with_a_uuid_session() {
    let h = harness().await;
    hook(&h, "session-start", "codex", CODEX_THREAD).await;
    hook(&h, "session-start", "claude-code", CLAUDE_SESSION).await;
    // No handle kind for grok; a Codex session id the server must hash.
    hook(
        &h,
        "session-start",
        "grok",
        "6a1d2c3b-4a59-4687-9786-a5b4c3d2e1f0",
    )
    .await;
    hook(&h, "session-start", "codex", "codex-named-session").await;
    until_handles(&h, 2).await;
    // Let the other two events land before asserting they added nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut found: Vec<(&'static str, String, &'static str, bool)> = handles(&h)
        .await
        .into_iter()
        .map(|(agent, handle)| {
            (
                agent.as_str(),
                handle.handle,
                handle.kind.as_str(),
                handle.open,
            )
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [
            (
                "claude-code",
                CLAUDE_SESSION.to_owned(),
                "claude_session",
                true
            ),
            ("codex", CODEX_THREAD.to_owned(), "codex_thread", true),
        ]
    );
}

#[tokio::test]
async fn the_bus_lists_addressed_mail_of_agents_with_a_handle_and_no_text() {
    let h = harness().await;
    hook(&h, "session-start", "codex", CODEX_THREAD).await;
    until_handles(&h, 1).await;
    let to_codex = send_to(&h, Some(AgentKind::Codex)).await;
    // Shared mail and mail for a harness without a handle make no target.
    send_to(&h, None).await;
    send_to(&h, Some(AgentKind::Grok)).await;

    let (status, json) = bus(&h, "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let text = json.to_string();
    for canary in ["SUBJECT-CANARY", "BODY-CANARY", "claude-code"] {
        assert!(!text.contains(canary), "{canary} leaked: {text}");
    }
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1, "{json}");
    let agent = &agents[0];
    assert_eq!(agent["workspace"], WORKSPACE);
    assert_eq!(agent["project"], INBOX);
    assert_eq!(agent["agent"], "codex");
    assert_eq!(agent["handle"]["kind"], "codex_thread");
    assert_eq!(agent["handle"]["id"], CODEX_THREAD);
    assert_eq!(agent["handle"]["open"], true);
    assert_eq!(agent["pending"], json!([to_codex]));
    assert_eq!(agent["truncated"], false);

    // The newest open session wins; an ended one is listed only when no
    // session of the harness is open.
    hook(&h, "session-start", "codex", NEWER_THREAD).await;
    until_handles(&h, 2).await;
    let (_, json) = bus(&h, "").await;
    assert_eq!(json["agents"][0]["handle"]["id"], NEWER_THREAD, "{json}");
    hook(&h, "session-end", "codex", NEWER_THREAD).await;
    hook(&h, "session-end", "codex", CODEX_THREAD).await;
    let mut ended = Value::Null;
    for _ in 0..250 {
        let (_, json) = bus(&h, "").await;
        if json["agents"][0]["handle"]["open"] == false {
            ended = json;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(ended["agents"][0]["handle"]["open"], false, "{ended}");
}

#[tokio::test]
async fn the_bus_long_poll_wakes_on_a_send_to_any_inbox() {
    let h = harness().await;
    hook(&h, "session-start", "codex", CODEX_THREAD).await;
    until_handles(&h, 1).await;
    let (_, first) = bus(&h, "").await;
    assert_eq!(first["agents"], json!([]));
    let cursor = first["cursor"].as_str().unwrap().to_owned();

    let writer = h.state.writer.clone();
    let inbox = h.inbox;
    let sender = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        writer
            .insert_message(NewAgentMessage {
                from_workspace_id: inbox.0,
                from_project_id: inbox.1,
                from_agent: AgentKind::ClaudeCode,
                from_session_id: None,
                from_owner_user: None,
                to_workspace_id: inbox.0,
                to_project_id: inbox.1,
                to_agent: Some(AgentKind::Codex),
                subject: None,
                body: "wake".into(),
                nature: MessageNature::Unknown,
                evidence: None,
                priority: ai_memory_core::MessagePriority::Normal,
                notify: ai_memory_core::NotifyOn::DEFAULT,
                ttl_secs: None,
            })
            .await
            .unwrap()
    });
    let started = std::time::Instant::now();
    let (status, json) = bus(&h, &format!("after={cursor}&wait_ms=10000")).await;
    let sent = sender.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the send woke the bus, not the timeout"
    );
    assert_eq!(json["agents"][0]["pending"], json!([sent.to_string()]));
    assert_ne!(json["cursor"].as_str().unwrap(), cursor);
}

#[tokio::test]
async fn the_bus_needs_the_marker_the_root_credential_and_a_known_query() {
    let h = harness().await;
    let (status, _) = call(&h.app, bus_request("", false)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no client marker");

    let mut as_user = bus_request("", true);
    as_user
        .extensions_mut()
        .insert(AuthorizedViewer(UserId::new()));
    let (status, body) = call(&h.app, as_user).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let mut user_level = bus_request("", true);
    user_level.extensions_mut().insert(AuthLevel::User);
    let (status, _) = call(&h.app, user_level).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a database user's level");

    let mut root = bus_request("", true);
    root.extensions_mut().insert(AuthLevel::Root);
    let (status, _) = call(&h.app, root).await;
    assert_eq!(status, StatusCode::OK, "the root credential");

    let (status, _) = bus(&h, "workspace=x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown parameter");
    let (status, _) = bus(&h, "wait_ms=25001").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "wait above the cap");
}

/// `GET /inbox/bus/status` and `POST /inbox/bus/program` are root-only like
/// the bus, the status names agents without session ids, and a program
/// report is audited only when every field is in its vocabulary.
#[tokio::test]
async fn the_bus_status_and_program_reports_are_root_only_and_bounded() {
    let h = harness().await;
    hook(&h, "session-start", "codex", CODEX_THREAD).await;
    for _ in 0..100 {
        if !handles(&h).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status_request = |marker: bool| {
        let mut builder = Request::get("/inbox/bus/status");
        if marker {
            builder = builder.header(crate::CLIENT_MARKER_HEADER, "cli");
        }
        builder.body(Body::empty()).unwrap()
    };
    let (status, _) = call(&h.app, status_request(false)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no client marker");
    let mut as_user = status_request(true);
    as_user
        .extensions_mut()
        .insert(AuthorizedViewer(UserId::new()));
    let (status, _) = call(&h.app, as_user).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a database user");
    let (status, body) = call(&h.app, status_request(true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["agents"][0]["agent"], "codex", "{json}");
    assert_eq!(json["agents"][0]["open"], true);
    assert!(!body.contains(CODEX_THREAD), "a session id leaked: {body}");

    let program = |body: Value| {
        Request::post("/inbox/bus/program")
            .header(crate::CLIENT_MARKER_HEADER, "cli")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let sha = "a".repeat(64);
    let good = json!({"adapter": "claude_resume", "program": "/home/u/.local/bin/claude",
                      "sha256": sha, "trusted": "first_use"});
    for bad in [
        json!({"adapter": "shell", "program": "/bin/sh", "sha256": sha, "trusted": "first_use"}),
        json!({"adapter": "claude_resume", "program": "claude", "sha256": sha, "trusted": "first_use"}),
        json!({"adapter": "claude_resume", "program": "/x", "sha256": "zz", "trusted": "first_use"}),
        json!({"adapter": "claude_resume", "program": "/x", "sha256": sha, "trusted": "approved"}),
        json!({"adapter": "claude_resume", "program": "/x", "sha256": sha, "trusted": "changed",
               "extra": 1}),
    ] {
        let (status, body) = call(&h.app, program(bad.clone())).await;
        assert!(status.is_client_error(), "{bad}: {status} {body}");
    }
    let (status, body) = call(&h.app, program(good)).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, body) = call(&h.app, status_request(true)).await;
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["programs"][0]["program"]["program"], "/home/u/.local/bin/claude",
        "{json}"
    );
}
