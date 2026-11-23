//! The root-only `/web/chat` page, its long-poll and the routes behind its
//! buttons.
//!
//! Pinned here: the page and every route refuse a database user; text that
//! agents wrote is escaped and never becomes markup, on the page and in the
//! long-poll's fragment; another project's text is shown only for mail sent
//! while a link joined the two; a paused pair gets a Resume button; the
//! long-poll answers at once on a change, waits otherwise, and wakes on new
//! mail; a human post lands in a room, mentions mail the agent, and resumes
//! paused pairs; and a link needs the destination's confirmation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_memory_core::{
    AgentKind, AuthLevel, MessageNature, MessagePriority, NewAgentMessage, NewUser, NotifyOn,
    ProjectId, UserRole, WorkspaceId,
};
use ai_memory_store::{LinkRequester, NewLinkRequest, ProjectKey, Store};
use ai_memory_web::{
    HtmlAuthRedirectConfig, Recall, WebMountSpec, html_auth_redirect_mw, split_web_routers,
};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const CANARY: &str = "<script>alert('BODY-CANARY')</script>";

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

async fn send(router: axum::Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: &Value, ui: bool) -> Request<Body> {
    let mut builder = Request::post(uri).header("content-type", "application/json");
    if ui {
        builder = builder.header("x-ai-memory-ui", "1");
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

struct World {
    store: Store,
    wiki: Wiki,
    a: (WorkspaceId, ProjectId),
    b: (WorkspaceId, ProjectId),
    _tmp: TempDir,
}

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let a = store
        .writer
        .get_or_create_project(ws, "api".to_string(), None)
        .await
        .unwrap();
    let b = store
        .writer
        .get_or_create_project(ws, "web".to_string(), None)
        .await
        .unwrap();
    World {
        store,
        wiki,
        a: (ws, a),
        b: (ws, b),
        _tmp: tmp,
    }
}

fn key(at: (WorkspaceId, ProjectId)) -> ProjectKey {
    ProjectKey {
        workspace_id: at.0,
        project_id: at.1,
    }
}

async fn mail(
    world: &World,
    from: (WorkspaceId, ProjectId),
    from_agent: AgentKind,
    to: (WorkspaceId, ProjectId),
    to_agent: AgentKind,
    body: &str,
) {
    world
        .store
        .writer
        .insert_message(NewAgentMessage {
            from_workspace_id: from.0,
            from_project_id: from.1,
            from_agent,
            from_session_id: None,
            from_owner_user: None,
            to_workspace_id: to.0,
            to_project_id: to.1,
            to_agent: Some(to_agent),
            subject: Some("a subject".into()),
            body: body.into(),
            nature: MessageNature::Unknown,
            evidence: None,
            priority: MessagePriority::Normal,
            notify: NotifyOn::DEFAULT,
            ttl_secs: None,
        })
        .await
        .unwrap();
}

async fn make_a_database_user(world: &World) {
    world
        .store
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
}

fn root(world: &World) -> axum::Router {
    app(&world.store, &world.wiki, AuthLevel::Root)
}

#[tokio::test]
async fn the_page_and_every_route_are_root_only() {
    let w = world().await;
    mail(
        &w,
        w.a,
        AgentKind::ClaudeCode,
        w.a,
        AgentKind::Codex,
        "SECRET-TEXT",
    )
    .await;
    make_a_database_user(&w).await;
    let user = || app(&w.store, &w.wiki, AuthLevel::User);
    for request in [
        get("/web/chat"),
        get("/web/chat?tab=cross"),
        get("/web/chat/watch?workspace=default&project=api&wait_ms=0"),
        post(
            "/web/chat/post",
            &json!({"workspace": "default", "project": "api", "room": "general", "body": "hi"}),
            true,
        ),
        post(
            "/web/chat/links",
            &json!({"from_workspace": "default", "from_project": "api",
                    "to_workspace": "default", "to_project": "web", "reason": "x"}),
            true,
        ),
    ] {
        let uri = request.uri().to_string();
        let (status, body) = send(user(), request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert!(!body.contains("SECRET-TEXT"), "{uri} leaked text");
    }
    assert_eq!(
        w.store
            .reader
            .agent_links(None, true, 10)
            .await
            .unwrap()
            .len(),
        0
    );
    let (status, _) = send(root(&w), get("/web/chat")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn agent_text_is_escaped_and_cut_on_the_page_and_in_the_fragment() {
    let w = world().await;
    mail(
        &w,
        w.a,
        AgentKind::ClaudeCode,
        w.a,
        AgentKind::Codex,
        CANARY,
    )
    .await;
    mail(
        &w,
        w.a,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        &format!("{}END-OF-LONG", "long ".repeat(600)),
    )
    .await;
    w.store
        .writer
        .room_post(ai_memory_store::NewRoomPost {
            workspace_id: w.a.0,
            project_id: w.a.1,
            room: ai_memory_core::rooms::RoomName::parse("review").unwrap(),
            from_agent: AgentKind::GeminiCli,
            from_session_id: None,
            from_owner_user: None,
            body: "post <img src=x onerror=alert(1)> \u{202e}tail".into(),
        })
        .await
        .unwrap();

    let (status, page) = send(root(&w), get("/web/chat?workspace=default&project=api")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("&lt;script&gt;alert(&#x27;BODY-CANARY&#x27;)&lt;/script&gt;"),
        "{page}"
    );
    assert!(!page.contains(CANARY), "the body became markup");
    assert!(!page.contains("<img src=x"), "a post became markup");
    assert!(
        !page.contains('\u{202e}'),
        "a bidi override reached the page"
    );
    assert!(!page.contains("END-OF-LONG"), "a long body was not cut");
    assert!(page.contains("data-tone=\"claude\"") && page.contains("data-tone=\"codex\""));
    assert!(
        page.contains("data-tone=\"gemini\""),
        "one colour per harness"
    );
    assert!(page.contains("data-kind=\"post\"") && page.contains("#review"));

    let (status, body) = send(
        root(&w),
        get("/web/chat/watch?workspace=default&project=api&wait_ms=0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    let html = json["html"].as_str().unwrap();
    assert!(html.contains("&lt;script&gt;"), "{html}");
    assert!(!html.contains(CANARY));
    assert!(!html.contains("<img src=x"));
}

#[tokio::test]
async fn another_projects_text_shows_only_while_a_link_joins_the_two() {
    let w = world().await;
    mail(
        &w,
        w.b,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        "BEFORE-LINK-TEXT",
    )
    .await;
    let (_, page) = send(root(&w), get("/web/chat?workspace=default&project=api")).await;
    assert!(
        !page.contains("BEFORE-LINK-TEXT"),
        "text of an unlinked project"
    );
    assert!(page.contains("default/web"), "who and where stay visible");
    assert!(page.contains("data-cross=\"1\""));

    tokio::time::sleep(Duration::from_millis(3)).await;
    w.store
        .writer
        .link_request(NewLinkRequest {
            from: key(w.a),
            to: key(w.b),
            reason: "share".into(),
            valid_secs: 7_200,
            by: LinkRequester::Human,
            confirm_now: true,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    mail(
        &w,
        w.b,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        "DURING-LINK-TEXT",
    )
    .await;
    let (_, page) = send(root(&w), get("/web/chat?workspace=default&project=api")).await;
    assert!(page.contains("DURING-LINK-TEXT"), "{page}");
    assert!(
        !page.contains("BEFORE-LINK-TEXT"),
        "mail from before the link stays hidden"
    );
}

#[tokio::test]
async fn a_paused_pair_shows_a_resume_button_and_delivery_state_shows_on_mail() {
    let w = world().await;
    for n in 1..=3 {
        mail(
            &w,
            w.a,
            AgentKind::ClaudeCode,
            w.a,
            AgentKind::Codex,
            &format!("Build failed: retry {n} of job 1234567{n}"),
        )
        .await;
    }
    let (_, page) = send(root(&w), get("/web/chat?workspace=default&project=api")).await;
    assert!(page.contains("data-pair=\"repeated_content\""), "{page}");
    assert!(page.contains("data-resume=\""), "{page}");
    assert!(
        page.contains("chat-delivery"),
        "delivery state on each mail"
    );
    // The other project's page has no banner for a pair it is not part of.
    let (_, other) = send(root(&w), get("/web/chat?workspace=default&project=web")).await;
    assert!(!other.contains("data-pair="), "{other}");
}

#[tokio::test]
async fn the_long_poll_answers_on_change_waits_otherwise_and_wakes_on_mail() {
    let w = world().await;
    let uri = "/web/chat/watch?workspace=default&project=api";
    let (status, first) = send(root(&w), get(&format!("{uri}&wait_ms=0"))).await;
    assert_eq!(status, StatusCode::OK);
    let first: Value = serde_json::from_str(&first).unwrap();
    let cursor = first["cursor"].as_str().unwrap().to_owned();
    assert!(first["html"].is_string(), "wait_ms=0 always answers");

    let (status, _) = send(root(&w), get(&format!("{uri}&wait_ms=99999"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        root(&w),
        get("/web/chat/watch?workspace=default&project=nope&wait_ms=0"),
    )
    .await;
    assert_ne!(status, StatusCode::OK);

    // Nothing changes: it waits, then says nothing changed.
    let started = Instant::now();
    let (_, quiet) = send(root(&w), get(&format!("{uri}&after={cursor}&wait_ms=1200"))).await;
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "it did not wait"
    );
    let quiet: Value = serde_json::from_str(&quiet).unwrap();
    assert_eq!(quiet["cursor"], cursor.as_str());
    assert!(quiet["html"].is_null());

    // New mail wakes the wait well before it ends.
    let router = root(&w);
    let waiting = tokio::spawn({
        let request = get(&format!("{uri}&after={cursor}&wait_ms=20000"));
        async move { send(router, request).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent_at = Instant::now();
    mail(
        &w,
        w.a,
        AgentKind::ClaudeCode,
        w.a,
        AgentKind::Codex,
        "NEW-MAIL-TEXT",
    )
    .await;
    let (status, woke) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert!(
        sent_at.elapsed() < Duration::from_secs(5),
        "it slept through the mail"
    );
    let woke: Value = serde_json::from_str(&woke).unwrap();
    assert_ne!(woke["cursor"], cursor.as_str());
    assert!(woke["html"].as_str().unwrap().contains("NEW-MAIL-TEXT"));
}

#[tokio::test]
async fn a_human_post_lands_in_the_room_mentions_the_agent_and_resumes_paused_pairs() {
    let w = world().await;
    for n in 1..=3 {
        mail(
            &w,
            w.a,
            AgentKind::ClaudeCode,
            w.a,
            AgentKind::Codex,
            &format!("Build failed: retry {n} of job 1234567{n}"),
        )
        .await;
    }
    assert_eq!(
        w.store.reader.paused_agent_pairs(10).await.unwrap().len(),
        1
    );
    let body = json!({
        "workspace": "default", "project": "api", "room": "general",
        "body": "@codex please take this one",
    });
    let (status, _) = send(root(&w), post("/web/chat/post", &body, false)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no UI header");
    let (status, _) = send(
        root(&w),
        Request::post("/web/chat/post")
            .header("content-type", "application/json")
            .header("x-ai-memory-ui", "1")
            .header("origin", "https://evil.example")
            .header("host", "localhost")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a foreign origin");

    let (status, answer) = send(root(&w), post("/web/chat/post", &body, true)).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["seq"], 1);
    assert_eq!(answer["mentioned"], json!(["codex"]));
    assert!(
        w.store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty(),
        "a human post resumes the paused pair"
    );
    let (_, page) = send(root(&w), get("/web/chat?workspace=default&project=api")).await;
    assert!(page.contains("@codex please take this one"));
    assert!(
        page.contains("data-side=\"end\""),
        "the human sits on the other side"
    );
    assert!(page.contains("data-tone=\"human\""));

    for bad in [
        json!({"workspace": "default", "project": "api", "room": "Bad Room", "body": "x"}),
        json!({"workspace": "default", "project": "api", "room": "general", "body": "   "}),
        json!({"workspace": "default", "project": "api", "room": "general", "body": "x".repeat(4001)}),
        json!({"workspace": "default", "project": "nope", "room": "general", "body": "x"}),
    ] {
        let (status, _) = send(root(&w), post("/web/chat/post", &bad, true)).await;
        assert_ne!(status, StatusCode::CREATED, "{bad}");
    }
}

#[tokio::test]
async fn a_link_needs_the_destinations_confirmation_and_either_page_can_end_it() {
    let w = world().await;
    let request = json!({
        "from_workspace": "default", "from_project": "api",
        "to_workspace": "default", "to_project": "web",
        "reason": "share the schema", "hours": 2,
    });
    let (status, answer) = send(root(&w), post("/web/chat/links", &request, true)).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["state"], "requested", "a request opens nothing");
    let id = answer["id"].as_str().unwrap().to_owned();

    // The requester's page shows it but cannot confirm it; the destination's
    // page offers Confirm.
    let (_, from_page) = send(
        root(&w),
        get("/web/chat?tab=cross&workspace=default&project=api"),
    )
    .await;
    assert!(from_page.contains("data-link=\""), "{from_page}");
    assert!(
        !from_page.contains("data-link-confirm=\""),
        "the requester can confirm"
    );
    let (_, to_page) = send(
        root(&w),
        get("/web/chat?tab=cross&workspace=default&project=web"),
    )
    .await;
    assert!(
        to_page.contains(&format!("data-link-confirm=\"{id}\"")),
        "{to_page}"
    );
    let (_, to_chat) = send(root(&w), get("/web/chat?workspace=default&project=web")).await;
    assert!(
        to_chat.contains("data-link-request="),
        "a banner waits on the destination"
    );

    let confirm = |project: &str| {
        post(
            &format!("/web/chat/links/{id}/confirm"),
            &json!({"workspace": "default", "project": project}),
            true,
        )
    };
    let (status, refused) = send(root(&w), confirm("api")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.contains("not_destination"));
    assert!(
        !w.store
            .reader
            .projects_linked_at(w.a.1, w.b.1, jiff::Timestamp::now().as_microsecond())
            .await
            .unwrap()
    );
    let (status, done) = send(root(&w), confirm("web")).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert!(
        w.store
            .reader
            .projects_linked_at(w.a.1, w.b.1, jiff::Timestamp::now().as_microsecond())
            .await
            .unwrap()
    );
    let (status, _) = send(root(&w), confirm("web")).await;
    assert_eq!(status, StatusCode::CONFLICT, "confirming twice");

    let (status, _) = send(
        root(&w),
        post(&format!("/web/chat/links/{id}/close"), &json!({}), true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !w.store
            .reader
            .projects_linked_at(w.a.1, w.b.1, jiff::Timestamp::now().as_microsecond())
            .await
            .unwrap()
    );
    let (status, _) = send(
        root(&w),
        post(&format!("/web/chat/links/{id}/close"), &json!({}), true),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "already over");

    for bad in [
        json!({"from_workspace": "default", "from_project": "api", "to_workspace": "default",
               "to_project": "web", "reason": "x", "hours": 25}),
        json!({"from_workspace": "default", "from_project": "api", "to_workspace": "default",
               "to_project": "web", "reason": "  "}),
        json!({"from_workspace": "default", "from_project": "api", "to_workspace": "default",
               "to_project": "api", "reason": "x"}),
    ] {
        let (status, _) = send(root(&w), post("/web/chat/links", &bad, true)).await;
        assert_ne!(status, StatusCode::CREATED, "{bad}");
    }
    // The form's own shortcut: the root user is the user of both.
    let both = json!({
        "from_workspace": "default", "from_project": "api",
        "to_workspace": "default", "to_project": "web",
        "reason": "same person", "confirm_now": true,
    });
    let (status, answer) = send(root(&w), post("/web/chat/links", &both, true)).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert!(answer.contains("\"active\""), "{answer}");
}

#[tokio::test]
async fn the_rooms_page_stays_reachable_by_tab_and_the_menu_points_at_the_chat() {
    let w = world().await;
    let (_, rooms) = send(root(&w), get("/web/rooms")).await;
    assert!(rooms.contains("data-chat-tab=\"chat\"") && rooms.contains("data-chat-tab=\"rooms\""));
    let (_, chat) = send(root(&w), get("/web/chat")).await;
    assert!(chat.contains("data-chat-tab=\"rooms\""), "{chat}");
    assert!(chat.contains("href=\"rooms\""));
    assert!(chat.contains("data-nav=\"chat\""));
    // Both selects the page uses are plain selects, so the one design-system
    // rule styles them; the page adds no select style of its own.
    let css = include_str!("../../static/tailwind.css");
    assert!(!css.contains("chat-project-picker select{-webkit-appearance"));
    assert!(
        css.contains(".chat-bubble{"),
        "the chat styles are in the built stylesheet"
    );
}
