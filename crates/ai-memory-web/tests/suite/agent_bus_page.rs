//! The root-only `GET /agent-bus` page and its resume button: paused pairs
//! are listed without message text, a resume needs the UI header and the
//! root credential, and it reopens wakes for that pair.

use std::sync::Arc;

use ai_memory_core::{AgentKind, AuthLevel, MessageNature, NewAgentMessage};
use ai_memory_store::Store;
use ai_memory_web::{
    HtmlAuthRedirectConfig, Recall, WebMountSpec, html_auth_redirect_mw, split_web_routers,
};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

fn app(store: &Store, wiki: &Wiki, level: AuthLevel) -> axum::Router {
    let cfg = Arc::new(HtmlAuthRedirectConfig::from_mount("", "/web"));
    split_web_routers(
        true,
        store.reader.clone(),
        wiki.clone(),
        WebMountSpec {
            enable_api: false,
            web_ui_dir: None,
            cors_origins: &[],
            web_slug: "/web",
            base_href: "/web/",
            base_path: "",
            trusted_proxy_identity: false,
            recall: Recall::default(),
            runtime: None,
            health: None,
        },
    )
    .unwrap()
    .protected
    .layer(axum::middleware::from_fn(
        move |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(level);
            next.run(req).await
        },
    ))
    .layer(axum::middleware::from_fn_with_state(
        cfg,
        html_auth_redirect_mw,
    ))
}

async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let resp = app.oneshot(request).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn page() -> Request<Body> {
    Request::builder()
        .uri("/web/agent-bus")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn paused_pairs_are_listed_without_text_and_resumed_only_by_root_from_the_page() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "api".to_string(), None)
        .await
        .unwrap();
    for _ in 0..3 {
        store
            .writer
            .insert_message(NewAgentMessage {
                from_workspace_id: ws,
                from_project_id: proj,
                from_agent: AgentKind::ClaudeCode,
                from_session_id: None,
                from_owner_user: None,
                to_workspace_id: ws,
                to_project_id: proj,
                to_agent: Some(AgentKind::Codex),
                subject: None,
                body: "BODY-CANARY same words".into(),
                nature: MessageNature::Unknown,
                evidence: None,
                priority: ai_memory_core::MessagePriority::Normal,
                notify: ai_memory_core::NotifyOn::DEFAULT,
                ttl_secs: None,
            })
            .await
            .unwrap();
    }

    let (status, body) = send(app(&store, &wiki, AuthLevel::Root), page()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("data-pair=\"repeated_content\""), "{body}");
    assert!(!body.contains("BODY-CANARY"), "message text on the page");
    let start = body.find("data-resume=\"").unwrap() + "data-resume=\"".len();
    let key = body[start..]
        .split('"')
        .next()
        .unwrap()
        .replace("&quot;", "\"")
        .replace("&#34;", "\"");
    let resume = |ui_header: bool| {
        let mut builder =
            Request::post("/web/agent-bus/resume").header("content-type", "application/json");
        if ui_header {
            builder = builder.header("x-ai-memory-ui", "1");
        }
        builder.body(Body::from(key.clone())).unwrap()
    };

    let (status, _) = send(app(&store, &wiki, AuthLevel::Root), resume(false)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no UI header");
    // With a human account on the server, a database user is no operator.
    store
        .writer
        .create_human_user(
            ai_memory_core::NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let (status, _) = send(app(&store, &wiki, AuthLevel::User), resume(true)).await;
    assert_ne!(status, StatusCode::OK, "a database user resumed a pair");
    assert_eq!(store.reader.paused_agent_pairs(10).await.unwrap().len(), 1);

    let (status, body) = send(app(&store, &wiki, AuthLevel::Root), resume(true)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"resumed\":true"), "{body}");
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
    let (_, body) = send(app(&store, &wiki, AuthLevel::Root), page()).await;
    assert!(body.contains("data-pairs=\"none\""), "{body}");

    // The sidebar's root-only summary: mail between agents not yet popped,
    // and paused pairs. A database user is refused, so the menu stays hidden.
    let summary = || {
        Request::builder()
            .uri("/web/agent-bus/summary")
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(app(&store, &wiki, AuthLevel::Root), summary()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["chat_unread"], 3);
    assert_eq!(json["paused_pairs"], 0);
    let (status, _) = send(app(&store, &wiki, AuthLevel::User), summary()).await;
    assert_ne!(status, StatusCode::OK);
}

/// Every page carries the hidden "Agents" menu (shown by the summary), and
/// every dashboard select gets the design-system look except the
/// management pages', which have their own.
#[tokio::test]
async fn the_agents_menu_is_in_every_page_and_selects_follow_the_design_system() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let (status, body) = send(
        app(&store, &wiki, AuthLevel::Root),
        Request::builder().uri("/web/inbox").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for needle in [
        "href=\"rooms\" data-nav=\"rooms\" data-nav-agents hidden",
        "href=\"agent-bus\" data-nav=\"agent-bus\" data-nav-agents hidden",
        "agent-bus/summary",
    ] {
        assert!(body.contains(needle), "{needle} missing");
    }
    let css = include_str!("../../static/tailwind.css");
    assert!(
        css.contains(".dashboard-shell select:not([multiple]):not(.management *){-webkit-appearance:none;"),
        "the shared select rule is missing from tailwind.css"
    );
    assert!(css.contains(".app-nav [data-nav-agents][hidden]{display:none}"));
}
