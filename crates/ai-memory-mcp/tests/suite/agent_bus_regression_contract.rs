//! Agent bus regression suite, MCP layer (`docs/agent-bus/TESTING.md`): the
//! tools an agent calls to use the bus, through the real `tools/call` and
//! `tools/list` path.
//!
//! This is the contract agents and plugins are written against. Pinned: the
//! exact set of `memory_message_*` and `memory_link_*` tools with the
//! parameter names and required fields of each; the shape of every answer an
//! agent parses (field names and value kinds, not the values); that mail
//! between two projects is delivered but a link request opens nothing; and
//! that the answers carry the security notice that tells an agent mail is
//! untrusted data.
//!
//! When a tool or an answer changes on purpose, update the table here AND
//! `docs/agent-bus/FOR_AGENTS.md`, the routing skills and the plugin skill
//! files in the same commit.

use std::collections::BTreeMap;

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

struct Harness {
    router: Router,
    store: Store,
    ws: ai_memory_core::WorkspaceId,
    projects: [ai_memory_core::ProjectId; 2],
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
        ws,
        projects: [a, b],
        _tmp: tmp,
    }
}

async fn rpc(router: &Router, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 8_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)))
}

/// A tool call whose answer is the JSON a tool returned.
async fn call(router: &Router, name: &str, arguments: Value) -> Value {
    let reply = rpc(
        router,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
    .await;
    assert!(reply.get("error").is_none(), "{name}: {reply}");
    let text = reply
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{name}: no text in {reply}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}: {text}"))
}

/// What an answer looks like, with every leaf replaced by its kind, objects
/// sorted by key and an array reduced to the shape of its first element.
fn shape(value: &Value) -> Value {
    match value {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        Value::Array(items) => match items.first() {
            Some(first) => json!([shape(first)]),
            None => json!([]),
        },
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
    }
}

/// `name -> (parameters, required)` for the tools whose name starts with one
/// of `prefixes`.
async fn tool_table(
    router: &Router,
    prefixes: &[&str],
) -> BTreeMap<String, (Vec<String>, Vec<String>)> {
    let listed = rpc(router, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("a tool list");
    tools
        .iter()
        .filter(|tool| {
            let name = tool["name"].as_str().unwrap();
            prefixes.iter().any(|prefix| name.starts_with(prefix))
        })
        .map(|tool| {
            let schema = &tool["inputSchema"];
            let mut parameters: Vec<String> = schema["properties"]
                .as_object()
                .map(|props| props.keys().cloned().collect())
                .unwrap_or_default();
            parameters.sort();
            let mut required: Vec<String> = schema["required"]
                .as_array()
                .map(|r| r.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                .unwrap_or_default();
            required.sort();
            (
                tool["name"].as_str().unwrap().to_owned(),
                (parameters, required),
            )
        })
        .collect()
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn agent_bus_regression_the_message_and_link_tools_and_their_parameters_are_pinned() {
    let h = harness().await;
    let table = tool_table(&h.router, &["memory_message_", "memory_link_"]).await;
    let expected: BTreeMap<String, (Vec<String>, Vec<String>)> = BTREE_EXPECTED
        .iter()
        .map(|(tool, parameters, required)| {
            ((*tool).to_owned(), (names(parameters), names(required)))
        })
        .collect();
    assert_eq!(
        table, expected,
        "a bus tool or one of its parameters changed. Update BTREE_EXPECTED, \
         docs/agent-bus/FOR_AGENTS.md, the routing skills (crates/ai-memory-core/src/routing_skills) \
         and plugins/*/skills/ai-memory-messaging together."
    );
}

/// `(tool, parameters, required)`.
const BTREE_EXPECTED: &[(&str, &[&str], &[&str])] = &[
    (
        "memory_link_close",
        &["agent", "link_id", "project", "workspace"],
        &["link_id"],
    ),
    (
        "memory_link_confirm",
        &["agent", "link_id", "project", "workspace"],
        &["link_id"],
    ),
    (
        "memory_link_list",
        &["agent", "include_ended", "project", "workspace"],
        &[],
    ),
    (
        "memory_link_request",
        &[
            "agent",
            "hours",
            "project",
            "reason",
            "to_project",
            "to_workspace",
            "workspace",
        ],
        &["reason", "to_project", "to_workspace"],
    ),
    (
        "memory_message_cancel",
        &["message_id", "project", "workspace"],
        &[],
    ),
    (
        "memory_message_list",
        &[
            "box",
            "include_settled",
            "limit",
            "message_id",
            "project",
            "workspace",
        ],
        &[],
    ),
    (
        "memory_message_pop",
        &["agent", "message_id", "project", "workspace"],
        &[],
    ),
    (
        "memory_message_send",
        &[
            "body",
            "evidence",
            "from_agent",
            "from_project",
            "from_workspace",
            "nature",
            "notify",
            "priority",
            "subject",
            "to_agent",
            "to_project",
            "to_workspace",
            "ttl_seconds",
        ],
        &["body", "to_project", "to_workspace"],
    ),
];

fn delivery(reason: &str, no_pop_alert_at: &str) -> Value {
    json!({
        "expires_at": "string", "no_pop_alert_at": no_pop_alert_at, "notify": "string",
        "reason": reason, "retry_after": "null", "state": "string",
    })
}

/// One message as a listing or a pop shows it.
fn message_shape(claimed: bool, receipt: Value) -> Value {
    json!({
        "body": "string", "claimed_at": if claimed { "string" } else { "null" },
        "created_at": "string",
        "delivery": delivery("null", if claimed { "null" } else { "string" }),
        "evidence": "null", "from_agent": "string", "from_owner_user": "null",
        "from_project_id": "string", "from_workspace_id": "string", "id": "string",
        "kind": "string", "nature": "string", "priority": "string", "receipt": receipt,
        "state": "string", "subject": "string", "to_agent": "string",
        "to_project_id": "string", "to_workspace_id": "string",
    })
}

fn link_shape() -> Value {
    json!({
        "close_reason": "null", "closed_at": "null", "closed_by": "null",
        "confirmed_at": "null", "confirmed_via": "null", "direction": "string",
        "ends_at": "string", "from": {"project": "string", "workspace": "string"},
        "id": "string", "reason": "string", "requested_at": "string",
        "requested_by": "string", "shown_to_an_agent": "bool",
        "state": "string", "to": {"project": "string", "workspace": "string"},
        "valid_hours": "number",
    })
}

#[tokio::test]
async fn agent_bus_regression_message_answers_keep_their_shape_and_cross_project_mail_is_delivered()
{
    let h = harness().await;
    // The recipient harness has run in project B, so the mail is queued, not
    // waiting for an unseen agent.
    h.store
        .writer
        .begin_session(ai_memory_core::NewSession {
            id: ai_memory_core::SessionId::new(),
            workspace_id: h.ws,
            project_id: h.projects[1],
            agent_kind: ai_memory_core::AgentKind::Codex,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    let to_b = |body: &str, extra: Value| {
        let mut args = json!({
            "from_workspace": WS, "from_project": A, "from_agent": "claude-code",
            "to_workspace": WS, "to_project": B, "to_agent": "codex",
            "subject": "a subject", "body": body, "notify": ["success"],
        });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };

    // Between two projects, with no link: accepted and queued.
    let sent = call(
        &h.router,
        "memory_message_send",
        to_b("BODY-CANARY hello", json!({})),
    )
    .await;
    assert_eq!(
        shape(&sent),
        json!({
            "delivery": delivery("null", "string"),
            "from_agent": "string", "message_id": "string", "to_agent": "string",
        }),
        "the answer to memory_message_send changed; agents parse it"
    );
    assert_eq!(sent["delivery"]["state"], "queued");
    let id = sent["message_id"].as_str().unwrap().to_owned();

    let inbox = call(
        &h.router,
        "memory_message_list",
        json!({"workspace": WS, "project": B, "agent": "codex"}),
    )
    .await;
    assert_eq!(
        shape(&inbox),
        json!({"messages": [message_shape(false, json!("null"))]})
    );
    assert_eq!(inbox["messages"][0]["id"], id.as_str());
    let outbox = call(
        &h.router,
        "memory_message_list",
        json!({"workspace": WS, "project": A, "box": "outbox"}),
    )
    .await;
    assert_eq!(
        shape(&outbox),
        json!({"messages": [message_shape(false, json!("null"))]})
    );

    // Reading it hands over the body inside a notice that says it is untrusted.
    let popped = call(
        &h.router,
        "memory_message_pop",
        json!({"workspace": WS, "project": B, "agent": "codex", "message_id": id}),
    )
    .await;
    assert_eq!(
        shape(&popped),
        json!({"message": message_shape(true, json!("null")), "security_notice": "string"})
    );
    assert_eq!(popped["message"]["body"], "BODY-CANARY hello");
    assert!(
        popped["security_notice"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("untrusted"),
        "{popped}"
    );
    let again = call(
        &h.router,
        "memory_message_pop",
        json!({"workspace": WS, "project": B, "agent": "codex", "message_id": id}),
    )
    .await;
    assert!(
        again.get("message").is_none_or(Value::is_null),
        "a second pop must hand over nothing: {again}"
    );

    // The sender asked for a receipt of success; it arrives as a receipt.
    let receipts = call(
        &h.router,
        "memory_message_list",
        json!({"workspace": WS, "project": A}),
    )
    .await;
    let receipt = &receipts["messages"][0];
    assert_eq!(receipt["kind"], "receipt", "{receipts}");
    assert_eq!(receipt["receipt"]["outcome"], "delivered");
    assert_eq!(receipt["receipt"]["receipt_for"], id.as_str());

    // Cancelling retracts mail that was not read yet.
    let second = call(
        &h.router,
        "memory_message_send",
        to_b("retract me", json!({})),
    )
    .await;
    let cancelled = call(
        &h.router,
        "memory_message_cancel",
        json!({"workspace": WS, "project": A, "message_id": second["message_id"]}),
    )
    .await;
    assert_eq!(shape(&cancelled), json!({"cancelled": "number"}));
    assert_eq!(cancelled["cancelled"], 1);
}

#[tokio::test]
async fn agent_bus_regression_link_answers_keep_their_shape_and_a_request_opens_nothing() {
    let h = harness().await;
    let filed = call(
        &h.router,
        "memory_link_request",
        json!({"workspace": WS, "project": A, "agent": "claude-code",
               "to_workspace": WS, "to_project": B, "reason": "share the schema"}),
    )
    .await;
    assert_eq!(
        shape(&filed),
        json!({
            "already_existed": "bool", "link": link_shape(), "next": "string",
            "security_notice": "string",
        }),
        "the answer to memory_link_request changed; agents parse it"
    );
    assert_eq!(
        filed["link"]["state"], "requested",
        "a request opens nothing"
    );
    assert_eq!(filed["link"]["direction"], "outgoing");
    assert!(
        filed["security_notice"]
            .as_str()
            .unwrap()
            .contains("untrusted")
    );
    let id = filed["link"]["id"].as_str().unwrap().to_owned();

    // The destination sees it waiting, and the list tells its agent what to do.
    let listed = call(
        &h.router,
        "memory_link_list",
        json!({"workspace": WS, "project": B, "agent": "codex"}),
    )
    .await;
    assert_eq!(
        shape(&listed),
        json!({
            "instructions": "string", "links": [link_shape()], "security_notice": "string",
            "waiting_for_your_project": [
                {"from_project": "string", "id": "string", "reason": "string",
                 "valid_hours": "number"}
            ],
        })
    );
    assert_eq!(listed["links"][0]["direction"], "incoming");

    // Neither side's agent confirms here: the requester is not the
    // destination, and a static client has no recorded session.
    for (project, agent, code) in [
        (A, "claude-code", "not_destination"),
        (B, "codex", "no_session"),
    ] {
        let refused = call(
            &h.router,
            "memory_link_confirm",
            json!({"workspace": WS, "project": project, "agent": agent, "link_id": id}),
        )
        .await;
        assert_eq!(
            shape(&refused),
            json!({"explanation": "string", "ok": "bool", "refused": "string"})
        );
        assert_eq!(
            (refused["ok"].clone(), refused["refused"].as_str()),
            (json!(false), Some(code))
        );
    }
    let still = call(
        &h.router,
        "memory_link_list",
        json!({"workspace": WS, "project": A, "agent": "claude-code"}),
    )
    .await;
    assert_eq!(still["links"][0]["state"], "requested");

    let closed = call(
        &h.router,
        "memory_link_close",
        json!({"workspace": WS, "project": A, "agent": "claude-code", "link_id": id}),
    )
    .await;
    assert_eq!(
        shape(&closed),
        json!({"closed": "bool", "next": "string", "was": "string"})
    );
    assert_eq!(
        (closed["closed"].clone(), closed["was"].as_str()),
        (json!(true), Some("requested"))
    );
}
