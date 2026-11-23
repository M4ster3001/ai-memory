//! The root-only `GET /rooms` page: it lists rooms, members and discovered
//! agents, never a post's text or a session id, and refuses a database user.

use std::sync::Arc;

use ai_memory_core::rooms::RoomName;
use ai_memory_core::{AgentKind, AuthLevel, NewUser, UserRole};
use ai_memory_store::{NewRoomPost, Store};
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

async fn get(app: axum::Router) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/web/rooms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn the_rooms_page_lists_rooms_without_text_and_is_root_only() {
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
        .get_or_create_project(ws, "proj-<b>x</b>".to_string(), None)
        .await
        .unwrap();
    store
        .writer
        .room_post(NewRoomPost {
            workspace_id: ws,
            project_id: proj,
            room: RoomName::parse("review").unwrap(),
            from_agent: AgentKind::ClaudeCode,
            from_session_id: None,
            from_owner_user: None,
            body: "BODY-CANARY @codex".into(),
        })
        .await
        .unwrap();

    let (status, page) = get(app(&store, &wiki, AuthLevel::Root)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("data-room=\"review\""), "{page}");
    assert!(page.contains("claude-code"));
    assert!(!page.contains("BODY-CANARY"), "post text on the page");
    assert!(!page.contains("<b>x</b>"), "an unescaped project name");

    store
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
    let (status, page) = get(app(&store, &wiki, AuthLevel::User)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        !page.contains("data-room="),
        "a database user saw the rooms"
    );
}
