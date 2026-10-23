//! Browser writes, directed delivery and scoped operational views.

use ai_memory_core::{
    AgentKind, AuthLevel, AuthorizedViewer, MessageBox, NewSession, NewUser, OwnerFilter,
    SessionId, TokenCall, UserRole,
};
use ai_memory_store::{GrantLevel, Store};
use ai_memory_web::router;
use ai_memory_wiki::Wiki;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn fixture() -> (
    tempfile::TempDir,
    Store,
    Wiki,
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
    SessionId,
) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: project,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    (tmp, store, wiki, ws, project, sid)
}

fn body() -> Value {
    json!({"from_workspace":"team","from_project":"app","to_workspace":"team","to_project":"app","to_agent":"codex","subject":"Review <script>test</script>","body":"Please inspect the large context."})
}
fn send() -> axum::http::request::Builder {
    Request::builder()
        .method("POST")
        .uri("/messages")
        .header("content-type", "application/json")
        .header("x-ai-memory-ui", "1")
}
async fn text(response: axum::response::Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn operations_project_tabs_have_one_default_panel_and_matching_controls() {
    let (_tmp, store, wiki, _, _, _) = fixture().await;
    let response = router(store.reader.clone(), wiki)
        .oneshot(
            Request::builder()
                .uri("/w/team/app")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert_eq!(
        html.matches("class=\"project-panel\" role=\"tabpanel\"")
            .count(),
        5
    );
    assert_eq!(html.matches("<a id=\"project-tab-").count(), 5);
    for tab in ["overview", "agents", "tokens", "sessions", "knowledge"] {
        let panel = html
            .split(&format!("<section id=\"project-panel-{tab}\""))
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert_eq!(panel.contains("hidden"), tab != "overview");
        assert!(panel.contains(&format!("aria-labelledby=\"project-tab-{tab}\"")));
        let control = html
            .split(&format!("<a id=\"project-tab-{tab}\""))
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(control.contains(&format!("href=\"w/team/app?tab={tab}\"")));
        assert!(control.contains(&format!("aria-controls=\"project-panel-{tab}\"")));
        assert!(control.contains(&format!("aria-selected=\"{}\"", tab == "overview")));
        assert_eq!(control.contains("tabindex=\"-1\""), tab != "overview");
    }
    let overview = html
        .split("id=\"project-panel-overview\"")
        .nth(1)
        .unwrap()
        .split("id=\"project-panel-agents\"")
        .next()
        .unwrap();
    assert!(overview.contains("Project capture summary") && overview.contains("Mailbox"));
    for detail in [
        "id=\"agents\"",
        "id=\"token-usage\"",
        "id=\"knowledge\"",
        "id=\"session-token-table\"",
    ] {
        assert!(!overview.contains(detail));
    }
}

#[tokio::test]
async fn operations_inbox_filters_initial_agents_by_both_project_coordinates() {
    let (_tmp, store, wiki, ws, _, _) = fixture().await;
    let other_ws = store.writer.get_or_create_workspace("other").await.unwrap();
    for (workspace, name, agent) in [
        (ws, "different", Some(AgentKind::ClaudeCode)),
        (other_ws, "app", Some(AgentKind::GeminiCli)),
        (ws, "empty", None),
    ] {
        let project = store
            .writer
            .get_or_create_project(workspace, name, None)
            .await
            .unwrap();
        if let Some(agent_kind) = agent {
            store
                .writer
                .begin_session(NewSession {
                    id: SessionId::new(),
                    workspace_id: workspace,
                    project_id: project,
                    agent_kind,
                    cwd: None,
                    actor_user: None,
                    occurred_at: None,
                })
                .await
                .unwrap();
        }
    }
    let app = router(store.reader.clone(), wiki);
    for (workspace, project, agent) in [
        ("team", "app", Some("codex")),
        ("team", "different", Some("claude-code")),
        ("other", "app", Some("gemini-cli")),
        ("team", "empty", None),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/inbox?workspace={workspace}&project={project}&agent={}",
                        agent.unwrap_or("codex")
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = text(response).await;
        let select = html
            .split("<select id=\"message-to\"")
            .nth(1)
            .unwrap()
            .split("</select>")
            .next()
            .unwrap();
        assert_eq!(
            select.matches("data-agent=").count(),
            usize::from(agent.is_some())
        );
        for candidate in ["codex", "claude-code", "gemini-cli"] {
            assert_eq!(
                select.contains(&format!("data-agent=\"{candidate}\"")),
                agent == Some(candidate),
                "{workspace}/{project}: {select}"
            );
        }
        let button = html
            .split("<button class=\"primary-button send-button\"")
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert_eq!(button.contains("disabled"), agent.is_none());
        if agent.is_some() {
            assert!(select.contains(&format!("data-workspace=\"{workspace}\"")));
            assert!(select.contains(&format!("data-project=\"{project}\"")));
            assert!(select.contains("selected"));
            assert!(!select.split('>').next().unwrap().contains("disabled"));
        } else {
            assert!(select.split('>').next().unwrap().contains("disabled"));
            assert!(select.contains("No registered agents in this project"));
        }
    }
}

#[tokio::test]
async fn operations_browser_can_send_to_an_explicit_other_project() {
    let (_tmp, store, wiki, ws, source, _) = fixture().await;
    let target_ws = store.writer.get_or_create_workspace("other").await.unwrap();
    let target = store
        .writer
        .get_or_create_project(target_ws, "app", None)
        .await
        .unwrap();
    store
        .writer
        .begin_session(NewSession {
            id: SessionId::new(),
            workspace_id: target_ws,
            project_id: target,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    let app = router(store.reader.clone(), wiki);
    let mut value = body();
    value["to_workspace"] = json!("other");
    value["to_agent"] = json!("claude-code");
    let response = app
        .oneshot(send().body(Body::from(value.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let mail = store
        .reader
        .list_messages(target_ws, target, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to_agent, Some(AgentKind::ClaudeCode));
    assert_eq!(mail[0].origin.from_workspace_id, ws);
    assert_eq!(mail[0].origin.from_project_id, source);
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, source)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn operations_browser_send_and_inbox_reads_preserve_pending_state() {
    let (_tmp, store, wiki, ws, project, _) = fixture().await;
    let app = router(store.reader.clone(), wiki);
    let response = app
        .clone()
        .oneshot(send().body(Body::from(body().to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let result: Value = serde_json::from_str(&text(response).await).unwrap();
    assert!(result["message_id"].is_string());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/inbox?workspace=team&project=app")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("Review &lt;script&gt;test&lt;/script&gt;"));
    assert!(!html.contains("Review <script>"));
    assert!(html.contains("Queued"));
    assert!(html.contains("Codex"));
    assert!(html.contains("Please inspect the large context."));
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, project)
            .await
            .unwrap(),
        1
    );
    let mail = store
        .reader
        .list_messages(ws, project, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(mail[0].to_agent, Some(AgentKind::Codex));
}

#[tokio::test]
async fn operations_browser_send_refuses_cross_origin_unknown_recipient_and_empty_body() {
    let (_tmp, store, wiki, ws, project, _) = fixture().await;
    let app = router(store.reader.clone(), wiki);
    let mut unknown = body();
    unknown["to_agent"] = json!("claude-code");
    let mut empty = body();
    empty["body"] = json!("");
    for value in [unknown, empty] {
        assert_eq!(
            app.clone()
                .oneshot(send().body(Body::from(value.to_string())).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        app.clone()
            .oneshot(
                send()
                    .header("host", "localhost:49374")
                    .header("origin", "https://foreign.example")
                    .body(Body::from(body().to_string()))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(body().to_string()))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, project)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn operations_message_write_obeys_root_user_and_project_grants() {
    let (_tmp, store, wiki, ws, project, _) = fixture().await;
    store
        .writer
        .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    store
        .writer
        .grant_memory(user, project, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = router(store.reader.clone(), wiki);
    let request = || {
        send()
            .extension(AuthLevel::User)
            .extension(AuthorizedViewer(user))
            .body(Body::from(body().to_string()))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(send().body(Body::from(body().to_string())).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    store
        .writer
        .grant_memory(user, project, GrantLevel::Write, None)
        .await
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        app.oneshot(
            send()
                .extension(AuthLevel::Root)
                .body(Body::from(body().to_string()))
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, project)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn operations_browser_messages_use_the_configured_sanitizer_and_admission() {
    use ai_memory_core::{SanitizeConfig, Sanitizer};
    use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig};
    let (_tmp, store, wiki, ws, project, _) = fixture().await;
    let wiki = wiki.with_sanitizer(
        Sanitizer::new(&SanitizeConfig {
            extra_patterns: vec!["PRIVATE_MARKER".into()],
            allowlist: Vec::new(),
        })
        .unwrap(),
    );
    let mut value = body();
    value["body"] = json!("Inspect PRIVATE_MARKER safely");
    value["subject"] = json!("PRIVATE_MARKER");
    assert_eq!(
        router(store.reader.clone(), wiki.clone())
            .oneshot(send().body(Body::from(value.to_string())).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    let mail = store
        .reader
        .list_messages(ws, project, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert!(!mail[0].body.contains("PRIVATE_MARKER"));
    assert!(!mail[0].subject.as_ref().unwrap().contains("PRIVATE_MARKER"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/deny", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/deny",
                axum::routing::post(|| async { StatusCode::FORBIDDEN }),
            ),
        )
        .await
        .unwrap()
    });
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "deny-browser-mail".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::MessageSend],
        blocking: true,
    }])
    .unwrap();
    let wiki = wiki
        .with_store_reader(store.reader.clone())
        .with_admission_chain(chain);
    assert_eq!(
        router(store.reader.clone(), wiki)
            .oneshot(send().body(Body::from(body().to_string())).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, project)
            .await
            .unwrap(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn operations_agent_totals_and_session_drilldown_do_not_multiply_tokens() {
    let (_tmp, store, wiki, ws, project, sid) = fixture().await;
    let calls: Vec<_> = (0..3)
        .map(|i| TokenCall {
            call_id: format!("call-{i}"),
            input_tokens: 100,
            output_tokens: 30,
            cache_read_tokens: 400,
            cache_write_tokens: 20,
            model: Some("test-model".into()),
        })
        .collect();
    store.writer.upsert_token_calls(sid, calls).await.unwrap();
    let report = store
        .reader
        .agent_usage(ws, project, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(report[0].input, 300);
    assert_eq!(report[0].calls, 3);
    assert_eq!(report[0].sessions, 1);
    let app = router(store.reader.clone(), wiki);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/w/team/app/agents/codex")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = text(response).await;
    assert!(html.contains("Codex"));
    assert!(html.contains(&sid.to_string()));
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/w/team/app/s/{sid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = text(response).await;
    assert!(html.contains("test-model"));
    assert!(html.contains("call-2"));
    let foreign = store
        .reader
        .session_token_usage(
            ai_memory_core::WorkspaceId::new(),
            project,
            sid,
            OwnerFilter::Any,
        )
        .await
        .unwrap();
    assert_eq!(foreign.call_count, 0);
}

#[tokio::test]
async fn operations_message_requires_write_access_to_both_project_coordinates() {
    let (_tmp, store, wiki, ws, source, _) = fixture().await;
    let target = store
        .writer
        .get_or_create_project(ws, "destination", None)
        .await
        .unwrap();
    store
        .writer
        .begin_session(NewSession {
            id: SessionId::new(),
            workspace_id: ws,
            project_id: target,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    for project in [source, target] {
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
    }
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: "writer".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    store
        .writer
        .grant_memory(user, source, GrantLevel::Write, None)
        .await
        .unwrap();
    store
        .writer
        .grant_memory(user, target, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = router(store.reader.clone(), wiki);
    for (from, to) in [("app", "destination"), ("destination", "app")] {
        let mut value = body();
        value["from_project"] = json!(from);
        value["to_project"] = json!(to);
        let response = app
            .clone()
            .oneshot(
                send()
                    .extension(AuthLevel::User)
                    .extension(AuthorizedViewer(user))
                    .body(Body::from(value.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, target)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .reader
            .pending_message_count(ws, source)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn operations_agent_aggregates_and_drilldowns_apply_ownership_before_totals() {
    let (_tmp, store, _wiki, ws, project, _) = fixture().await;
    let alice = ai_memory_core::IdentityKey::User("alice".into()).storage_key();
    let bob = ai_memory_core::IdentityKey::User("bob".into()).storage_key();
    let mut sessions = Vec::new();
    for (owner, tokens) in [(alice.clone(), 100), (bob, 900)] {
        let sid = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: sid,
                workspace_id: ws,
                project_id: project,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: Some(owner),
                occurred_at: None,
            })
            .await
            .unwrap();
        store
            .writer
            .upsert_token_calls(
                sid,
                vec![TokenCall {
                    call_id: sid.to_string(),
                    input_tokens: tokens,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    model: None,
                }],
            )
            .await
            .unwrap();
        sessions.push(sid);
    }
    let filter = OwnerFilter::User(alice);
    let totals = store
        .reader
        .agent_usage(ws, project, filter.clone())
        .await
        .unwrap();
    assert_eq!(totals[0].sessions, 2); // Shared legacy sessions remain readable.
    assert_eq!(totals[0].input, 100);
    assert_eq!(totals[0].calls, 1);
    let rows = store
        .reader
        .agent_sessions(ws, project, AgentKind::Codex, filter.clone(), 0)
        .await
        .unwrap();
    assert!(rows.iter().any(|s| s.session_id == sessions[0]));
    assert!(!rows.iter().any(|s| s.session_id == sessions[1]));
    let foreign = store
        .reader
        .session_token_usage(ws, project, sessions[1], filter)
        .await
        .unwrap();
    assert_eq!(foreign.call_count, 0);
}

#[tokio::test]
async fn operations_legacy_login_requires_an_explicit_nonpersistent_write_token() {
    let (_tmp, store, wiki, _, _, _) = fixture().await;
    let app = router(store.reader.clone(), wiki);
    for legacy in [false, true] {
        let mut request = Request::builder().uri("/inbox?workspace=team&project=app");
        if legacy {
            request = request.extension(ai_memory_core::LegacyBrowserAuth);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = text(response).await;
        assert_eq!(html.contains("id=\"message-access-token\""), legacy);
        if legacy {
            let input = html
                .split("<input id=\"message-access-token\"")
                .nth(1)
                .unwrap()
                .split('>')
                .next()
                .unwrap();
            assert!(
                input.contains("type=\"password\"")
                    && input.contains("autocomplete=\"off\"")
                    && input.contains("required")
            );
            assert!(!input.contains("value=") && !input.contains("name="));
        }
    }
}
