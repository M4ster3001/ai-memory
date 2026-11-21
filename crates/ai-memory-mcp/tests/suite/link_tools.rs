//! The `memory_link_*` tools through the real `tools/call` path.
//!
//! Pinned here: a request opens nothing and is idempotent; a static client
//! (no recorded session) can ask and list but never confirm; the requester's
//! project can never confirm; the destination's agent confirms only after it
//! was shown the request and its user answered in a prompt the server saw;
//! either side can close; and a refusal is an answer with a code, not a
//! protocol error.

use ai_memory_core::{
    AgentKind, NewObservation, NewSession, ObservationKind, OwnerFilter, Sanitized, Sanitizer,
    SessionId,
};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const A: &str = "project-a";
const B: &str = "project-b";

struct Harness {
    router: Router,
    store: Store,
    ids: [(ai_memory_core::WorkspaceId, ai_memory_core::ProjectId); 2],
    _tmp: TempDir,
}

async fn harness() -> Harness {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace(WS).await.unwrap();
    let a = store
        .writer
        .get_or_create_project(ws, A.to_string(), None)
        .await
        .unwrap();
    let b = store
        .writer
        .get_or_create_project(ws, B.to_string(), None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, a).with_wiki(wiki);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Harness {
        router: Router::new().nest_service("/mcp", service),
        store,
        ids: [(ws, a), (ws, b)],
        _tmp: tmp,
    }
}

/// One `tools/call`, optionally from a recorded hook session.
async fn call(
    router: &Router,
    name: &str,
    arguments: Value,
    session: Option<&str>,
) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        req = req.header("x-memory-actor-session-id", session);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    if let Some(err) = v.get("error") {
        return Err(err["message"].as_str().unwrap_or_default().to_owned());
    }
    let text = v
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if v.pointer("/result/isError") == Some(&Value::Bool(true)) {
        return Err(text.to_owned());
    }
    Ok(serde_json::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}")))
}

fn at(project: &str, agent: &str, extra: Value) -> Value {
    let mut args = json!({"workspace": WS, "project": project, "agent": agent});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    args
}

/// A user prompt in a hook session (which records the session).
async fn prompt(h: &Harness, project: usize, agent: AgentKind, session: &str, text: &str) {
    let at = h.ids[project];
    let id = SessionId::from_native(session);
    h.store
        .writer
        .admit_reported_hook_session_event(
            NewSession {
                id,
                workspace_id: at.0,
                project_id: at.1,
                agent_kind: agent,
                cwd: None,
                actor_user: None,
                occurred_at: None,
            },
            Sanitized::new(
                NewObservation {
                    occurred_at: None,
                    session_id: id,
                    workspace_id: at.0,
                    project_id: at.1,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: text.into(),
                    importance: 1,
                    file_path: None,
                },
                &Sanitizer::builtin(),
            ),
            OwnerFilter::Any,
            None,
            None,
            None,
        )
        .await
        .unwrap();
}

async fn pause() {
    tokio::time::sleep(Duration::from_millis(3)).await;
}

#[tokio::test]
async fn a_request_opens_nothing_and_a_static_client_can_never_confirm() {
    let h = harness().await;
    let request = |reason: &str| {
        at(
            A,
            "claude-code",
            json!({"to_workspace": WS, "to_project": B, "reason": reason}),
        )
    };
    let filed = call(
        &h.router,
        "memory_link_request",
        request("share the schema"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(filed["link"]["state"], "requested");
    assert_eq!(filed["link"]["direction"], "outgoing");
    assert_eq!(filed["already_existed"], false);
    assert!(
        filed["security_notice"]
            .as_str()
            .unwrap()
            .contains("untrusted")
    );
    let id = filed["link"]["id"].as_str().unwrap().to_owned();

    let again = call(&h.router, "memory_link_request", request("again"), None)
        .await
        .unwrap();
    assert_eq!(again["already_existed"], true);
    assert_eq!(again["link"]["id"], id.as_str());

    // The requester's project is not the destination.
    let own = call(
        &h.router,
        "memory_link_confirm",
        at(A, "claude-code", json!({"link_id": id})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(own["refused"], "not_destination", "{own}");

    // The destination, with no recorded session, sees the request but is told
    // it cannot confirm.
    let listed = call(
        &h.router,
        "memory_link_list",
        at(B, "codex", json!({})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(listed["waiting_for_your_project"][0]["id"], id.as_str());
    assert_eq!(listed["links"][0]["direction"], "incoming");
    assert!(
        listed["instructions"]
            .as_str()
            .unwrap()
            .contains("cannot confirm")
    );
    let blind = call(
        &h.router,
        "memory_link_confirm",
        at(B, "codex", json!({"link_id": id})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(blind["refused"], "no_session", "{blind}");
    assert_eq!(blind["ok"], false);

    // Nothing opened.
    let state = call(
        &h.router,
        "memory_link_list",
        at(A, "claude-code", json!({})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(state["links"][0]["state"], "requested");
}

#[tokio::test]
async fn the_destination_agent_confirms_after_showing_the_request_and_hearing_its_user() {
    let h = harness().await;
    prompt(&h, 1, AgentKind::Codex, "s-b", "carry on").await;
    let filed = call(
        &h.router,
        "memory_link_request",
        at(
            A,
            "claude-code",
            json!({"to_workspace": WS, "to_project": B, "reason": "need the schema", "hours": 1}),
        ),
        None,
    )
    .await
    .unwrap();
    let id = filed["link"]["id"].as_str().unwrap().to_owned();
    assert_eq!(filed["link"]["valid_hours"], 1.0);

    let confirm = || at(B, "codex", json!({"link_id": id}));
    let early = call(&h.router, "memory_link_confirm", confirm(), Some("s-b"))
        .await
        .unwrap();
    assert_eq!(early["refused"], "not_presented", "{early}");

    // Listing is the presentation.
    let shown = call(
        &h.router,
        "memory_link_list",
        at(B, "codex", json!({})),
        Some("s-b"),
    )
    .await
    .unwrap();
    assert!(
        shown["instructions"]
            .as_str()
            .unwrap()
            .contains("Show each to your user")
    );
    assert_eq!(
        shown["waiting_for_your_project"][0]["reason"],
        "need the schema"
    );
    let unanswered = call(&h.router, "memory_link_confirm", confirm(), Some("s-b"))
        .await
        .unwrap();
    assert_eq!(unanswered["refused"], "no_user_answer", "{unanswered}");

    pause().await;
    prompt(&h, 1, AgentKind::Codex, "s-b", "yes, connect them").await;
    let done = call(&h.router, "memory_link_confirm", confirm(), Some("s-b"))
        .await
        .unwrap();
    assert_eq!(done["confirmed"], true, "{done}");
    assert_eq!(done["link"]["state"], "active");
    assert_eq!(done["link"]["confirmed_via"], "agent");

    // The other side sees it active, and may end it.
    let from_a = call(
        &h.router,
        "memory_link_list",
        at(A, "claude-code", json!({})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(from_a["links"][0]["state"], "active");
    let closed = call(
        &h.router,
        "memory_link_close",
        at(A, "claude-code", json!({"link_id": id})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["was"], "active");
    let gone = call(
        &h.router,
        "memory_link_close",
        at(A, "claude-code", json!({"link_id": id})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(gone["refused"], "not_live");
    let history = call(
        &h.router,
        "memory_link_list",
        at(A, "claude-code", json!({"include_ended": true})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(history["links"][0]["state"], "closed");
}

#[tokio::test]
async fn bad_input_is_refused_before_anything_is_filed() {
    let h = harness().await;
    let base = |to: &str, reason: &str, hours: Value| {
        at(
            A,
            "codex",
            json!({"to_workspace": WS, "to_project": to, "reason": reason, "hours": hours}),
        )
    };
    for (args, why) in [
        (base("no-such-project", "x", json!(1)), "an unknown project"),
        (base(B, "   ", json!(1)), "an empty reason"),
        (base(B, &"x".repeat(301), json!(1)), "a long reason"),
        (base(B, "x", json!(25)), "more than 24 hours"),
        (base(B, "x", json!(0)), "no time at all"),
        (base(A, "x", json!(1)), "the same project"),
    ] {
        let answer = call(&h.router, "memory_link_request", args, None).await;
        let refused = match &answer {
            Err(_) => true,
            Ok(value) => value["refused"].is_string(),
        };
        assert!(refused, "{why}: {answer:?}");
    }
    let listed = call(
        &h.router,
        "memory_link_list",
        at(A, "codex", json!({})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(listed["links"], json!([]));
    let unknown_agent = call(
        &h.router,
        "memory_link_request",
        at(
            A,
            "not-an-agent",
            json!({"to_workspace": WS, "to_project": B, "reason": "x"}),
        ),
        None,
    )
    .await;
    assert!(unknown_agent.is_err());
}
