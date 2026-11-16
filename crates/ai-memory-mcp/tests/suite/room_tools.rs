//! The `memory_room_*` tools through the real `tools/call` path, as a static
//! MCP client drives them (explicit `workspace` + `project` + `agent`).
//!
//! Pinned here: a room is per project (the same name elsewhere is another
//! room and its posts never show there); a mention becomes inbox mail for the
//! mentioned harness only, without the post's text; a read returns new posts
//! once and then advances; a guessed project and an unknown harness are
//! refused; and every read carries the untrusted-input notice.

use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const A: &str = "project-a";
const B: &str = "project-b";

async fn harness() -> (Router, TempDir) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace(WS).await.unwrap();
    let baked = store
        .writer
        .get_or_create_project(ws, A.to_string(), None)
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, B.to_string(), None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked).with_wiki(wiki);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    (Router::new().nest_service("/mcp", service), tmp)
}

/// One `tools/call`: `Ok(payload)` or `Err(error message)`.
async fn call(router: &Router, name: &str, arguments: Value) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
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

#[tokio::test]
async fn rooms_are_per_project_and_a_mention_mails_only_the_mentioned_agent() {
    let (router, _tmp) = harness().await;
    let joined = call(
        &router,
        "memory_room_join",
        at(A, "codex", json!({"room": "#review"})),
    )
    .await
    .unwrap();
    assert_eq!(
        joined,
        json!({"room": "review", "agent": "codex", "created": true, "joined": true})
    );

    let posted = call(
        &router,
        "memory_room_post",
        at(
            A,
            "claude-code",
            json!({"room": "review", "body": "BODY-CANARY @codex please look"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(posted["seq"], 1);
    assert_eq!(posted["mentioned"][0]["agent"], "codex");
    let mention_id = posted["mentioned"][0]["message_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // The mention is codex's mail in project A, without the post's text.
    let inbox = call(
        &router,
        "memory_message_list",
        json!({"workspace": WS, "project": A}),
    )
    .await
    .unwrap();
    let text = inbox.to_string();
    assert!(text.contains(&mention_id), "{text}");
    assert!(!text.contains("BODY-CANARY"), "{text}");
    let other = call(
        &router,
        "memory_message_list",
        json!({"workspace": WS, "project": B}),
    )
    .await
    .unwrap();
    assert!(!other.to_string().contains(&mention_id));

    // Codex reads the post once; the second read is empty.
    let read = call(
        &router,
        "memory_room_read",
        at(A, "codex", json!({"room": "review"})),
    )
    .await
    .unwrap();
    assert_eq!(read["posts"].as_array().unwrap().len(), 1);
    assert_eq!(read["posts"][0]["from_agent"], "claude-code");
    assert_eq!(read["posts"][0]["body"], "BODY-CANARY @codex please look");
    assert!(
        read["security_notice"]
            .as_str()
            .unwrap()
            .contains("untrusted")
    );
    let again = call(
        &router,
        "memory_room_read",
        at(A, "codex", json!({"room": "review"})),
    )
    .await
    .unwrap();
    assert_eq!(again["posts"], json!([]));
    let history = call(
        &router,
        "memory_room_read",
        at(A, "codex", json!({"room": "review", "after": 0})),
    )
    .await
    .unwrap();
    assert_eq!(
        history["posts"].as_array().unwrap().len(),
        1,
        "after re-reads"
    );

    // The same name in project B is another, absent, room.
    assert!(
        call(
            &router,
            "memory_room_read",
            at(B, "codex", json!({"room": "review"}))
        )
        .await
        .is_err()
    );
    let listed = call(&router, "memory_room_list", at(B, "codex", json!({})))
        .await
        .unwrap();
    assert_eq!(listed["rooms"], json!([]));
    let listed = call(&router, "memory_room_list", at(A, "codex", json!({})))
        .await
        .unwrap();
    assert_eq!(
        listed["rooms"][0]["members"],
        json!(["codex", "claude-code"])
    );
    assert_eq!(listed["rooms"][0]["unread"], 0);

    let left = call(
        &router,
        "memory_room_leave",
        at(A, "codex", json!({"room": "review"})),
    )
    .await
    .unwrap();
    assert_eq!(left["left"], true);
}

#[tokio::test]
async fn a_room_needs_a_named_project_and_a_known_harness() {
    let (router, _tmp) = harness().await;
    // No project named: the baked default is a guess and is refused.
    let guessed = call(
        &router,
        "memory_room_post",
        json!({"room": "general", "agent": "codex", "body": "hi"}),
    )
    .await;
    assert!(guessed.is_err(), "{guessed:?}");
    for agent in ["claude", "other", "nobody"] {
        let refused = call(
            &router,
            "memory_room_post",
            at(A, agent, json!({"room": "general", "body": "hi"})),
        )
        .await;
        assert!(refused.is_err(), "{agent}: {refused:?}");
    }
    let no_agent = call(
        &router,
        "memory_room_post",
        json!({"workspace": WS, "project": A, "room": "general", "body": "hi"}),
    )
    .await;
    assert!(no_agent.is_err(), "{no_agent:?}");
    for room in ["General", "a b", "../x", ""] {
        assert!(
            call(
                &router,
                "memory_room_join",
                at(A, "codex", json!({"room": room}))
            )
            .await
            .is_err(),
            "{room:?}"
        );
    }
    // Control: the same post with everything named works.
    assert!(
        call(
            &router,
            "memory_room_post",
            at(A, "codex", json!({"room": "general", "body": "hi"}))
        )
        .await
        .is_ok()
    );
}
