//! The wake supervisor's routes through the real hook router:
//! `POST /inbox/wake-candidates` and `POST /inbox/wake-report`. Pinned here:
//! both need `Write` on the watched inbox and the client marker, refuse
//! unknown fields and values outside the enumerations, never return a
//! sender's name or any message text, and a report cannot reach mail outside
//! the reported inbox.

use ai_memory_core::{
    AgentKind, AuthorizedViewer, MessageNature, NewAgentMessage, NewSession, NewUser, SessionId,
    UserId, UserRole,
};
use ai_memory_store::{AccessMode, GrantLevel};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};

use super::inbox_watch::{Harness, INBOX, SENDER, WORKSPACE, call, harness};

fn post(path: &str, body: &Value, viewer: Option<UserId>, marker: bool) -> Request<Body> {
    let mut builder = Request::post(path).header("content-type", "application/json");
    if marker {
        builder = builder.header(crate::CLIENT_MARKER_HEADER, "cli");
    }
    let mut request = builder.body(Body::from(body.to_string())).unwrap();
    if let Some(viewer) = viewer {
        request.extensions_mut().insert(AuthorizedViewer(viewer));
    }
    request
}

async fn send(h: &Harness, verified: bool, to_agent: Option<AgentKind>) -> String {
    let from_session_id = if verified {
        let id = SessionId::new();
        h.state
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: h.sender.0,
                project_id: h.sender.1,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
                occurred_at: None,
            })
            .await
            .unwrap();
        Some(id)
    } else {
        None
    };
    h.state
        .writer
        .insert_message(NewAgentMessage {
            from_workspace_id: h.sender.0,
            from_project_id: h.sender.1,
            from_agent: AgentKind::ClaudeCode,
            from_session_id,
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

/// Mail sent from the watched project itself, by `from_agent`, with no
/// recorded session.
async fn send_inside_inbox(h: &Harness, from_agent: AgentKind) -> String {
    h.state
        .writer
        .insert_message(NewAgentMessage {
            from_workspace_id: h.inbox.0,
            from_project_id: h.inbox.1,
            from_agent,
            from_session_id: None,
            from_owner_user: None,
            to_workspace_id: h.inbox.0,
            to_project_id: h.inbox.1,
            to_agent: Some(AgentKind::Codex),
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

fn candidates_body(wake_from: &[(&str, &str)]) -> Value {
    json!({
        "workspace": WORKSPACE,
        "project": INBOX,
        "agent": "codex",
        "include_shared": true,
        "wake_from": wake_from
            .iter()
            .map(|(w, p)| json!({"workspace": w, "project": p}))
            .collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn candidates_classify_senders_without_naming_them() {
    let h = harness().await;
    let verified = send(&h, true, Some(AgentKind::Codex)).await;
    let named = send(&h, false, Some(AgentKind::Codex)).await;
    let shared = send(&h, true, None).await;
    let (status, body) = call(
        &h.app,
        post(
            "/inbox/wake-candidates",
            &candidates_body(&[(WORKSPACE, SENDER), ("team", "no-such-project")]),
            None,
            true,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    let senders: Vec<(&str, &str)> = json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["sender"].as_str().unwrap()))
        .collect();
    assert_eq!(
        senders,
        [
            (verified.as_str(), "listed"),
            (named.as_str(), "unverified"),
            (shared.as_str(), "listed"),
        ]
    );
    assert_eq!(json["unresolved_wake_from"], 1);
    for leaked in ["CANARY", SENDER, "claude-code"] {
        assert!(!body.contains(leaked), "{leaked} leaked: {body}");
    }
    // Without the sender listed, the same mail is unlisted.
    let (_, body) = call(
        &h.app,
        post("/inbox/wake-candidates", &candidates_body(&[]), None, true),
    )
    .await;
    assert_eq!(body.matches("\"unlisted\"").count(), 3, "{body}");
}

/// Mail inside the watched project is `same_project` without `wake_from` or
/// a recorded session, and the watched agent's mail to itself is
/// `same_agent` even when its own project is listed. Mail from another
/// project keeps the `wake_from` labels.
#[tokio::test]
async fn candidates_label_mail_inside_the_watched_project() {
    let h = harness().await;
    let colleague = send_inside_inbox(&h, AgentKind::ClaudeCode).await;
    let itself = send_inside_inbox(&h, AgentKind::Codex).await;
    let outsider = send(&h, false, Some(AgentKind::Codex)).await;
    for wake_from in [vec![], vec![(WORKSPACE, INBOX)]] {
        let (status, body) = call(
            &h.app,
            post(
                "/inbox/wake-candidates",
                &candidates_body(&wake_from),
                None,
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        let senders: Vec<(&str, &str)> = json["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["id"].as_str().unwrap(), m["sender"].as_str().unwrap()))
            .collect();
        assert_eq!(
            senders,
            [
                (colleague.as_str(), "same_project"),
                (itself.as_str(), "same_agent"),
                (outsider.as_str(), "unlisted"),
            ],
            "wake_from = {wake_from:?}"
        );
        for leaked in ["CANARY", SENDER, "claude-code"] {
            assert!(!body.contains(leaked), "{leaked} leaked: {body}");
        }
    }
}

#[tokio::test]
async fn reports_set_delivery_and_refuse_anything_outside_the_vocabulary() {
    let h = harness().await;
    let id = send(&h, true, Some(AgentKind::Codex)).await;
    let report = |extra: Value| {
        let mut body = json!({
            "workspace": WORKSPACE, "project": INBOX, "agent": "codex",
            "message_ids": [id.clone()],
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };
    let bad = [
        json!({"state": "delivered"}),
        json!({"state": "wake_failed"}),
        json!({"state": "wake_failed", "reason": "ttl"}),
        json!({"state": "wake_attempted", "retry_after_secs": 60}),
        json!({"state": "quota_exhausted", "reason": "quota_marker", "retry_after_secs": 0}),
        json!({"state": "wake_failed", "reason": "free text here"}),
        json!({"state": "wake_attempted", "note": "extra"}),
        json!({"state": "wake_attempted", "message_ids": []}),
        json!({"state": "wake_attempted", "message_ids": ["not-an-id"]}),
        json!({"state": "wake_attempted", "message_ids": [id.to_uppercase()]}),
        json!({"state": "wake_attempted", "agent": "Codex"}),
    ];
    for extra in bad {
        let (status, body) = call(
            &h.app,
            post("/inbox/wake-report", &report(extra.clone()), None, true),
        )
        .await;
        assert!(status.is_client_error(), "{extra}: {status} {body}");
    }
    let (status, _) = call(
        &h.app,
        post(
            "/inbox/wake-report",
            &report(json!({"state": "wake_attempted"})),
            None,
            false,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the client marker is required"
    );

    let (status, body) = call(
        &h.app,
        post(
            "/inbox/wake-report",
            &report(json!({"state": "quota_exhausted", "reason": "quota_marker", "retry_after_secs": 600})),
            None,
            true,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (json["updated"].as_u64(), json["receipts"].as_u64()),
        (Some(1), Some(1))
    );
}

/// Both routes act on the watched inbox, so a reader of it is refused and a
/// writer is served.
#[tokio::test]
async fn both_routes_need_write_on_the_watched_inbox() {
    let h = harness().await;
    h.state
        .writer
        .set_access_mode(h.inbox.1, AccessMode::Restricted)
        .await
        .unwrap();
    let id = send(&h, true, Some(AgentKind::Codex)).await;
    let user = |name: &'static str| {
        let writer = h.state.writer.clone();
        async move {
            writer
                .create_human_user(
                    NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap()
        }
    };
    let reader = user("reader").await;
    let writer = user("writer").await;
    h.state
        .writer
        .grant_memory(reader, h.inbox.1, GrantLevel::Read, None)
        .await
        .unwrap();
    h.state
        .writer
        .grant_memory(writer, h.inbox.1, GrantLevel::Write, None)
        .await
        .unwrap();
    let report = json!({
        "workspace": WORKSPACE, "project": INBOX, "agent": "codex",
        "message_ids": [id], "state": "wake_attempted",
    });
    for (path, body) in [
        (
            "/inbox/wake-candidates",
            candidates_body(&[(WORKSPACE, SENDER)]),
        ),
        ("/inbox/wake-report", report),
    ] {
        let (status, text) = call(&h.app, post(path, &body, Some(reader), true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {text}");
        assert!(!text.contains(&id_fragment(&body)), "{path}: {text}");
        let (status, text) = call(&h.app, post(path, &body, Some(writer), true)).await;
        assert_eq!(status, StatusCode::OK, "{path}: {text}");
    }
}

fn id_fragment(body: &Value) -> String {
    body["message_ids"][0]
        .as_str()
        .map_or_else(|| "\"listed\"".to_owned(), str::to_owned)
}
