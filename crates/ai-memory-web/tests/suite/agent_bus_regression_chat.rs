//! Agent bus regression suite, web layer (`docs/agent-bus/TESTING.md`): what
//! the root-only `/web/chat` page may show of text that agents wrote.
//!
//! Message bodies are written by other agents, possibly by an attacker who
//! got an agent to write them. Pinned here: HTML, bidirectional marks,
//! control characters and very long text are escaped, dropped or cut on the
//! page and in the long-poll fragment; another project's text is never
//! shown unless the mail was sent while a link joined the two; and only the
//! destination's page can confirm a link.

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::{
    AgentKind, AuthLevel, MessageNature, MessagePriority, NewAgentMessage, NotifyOn, ProjectId,
    WorkspaceId,
};
use ai_memory_store::{
    LinkCloser, LinkRequestOutcome, LinkRequester, NewLinkRequest, ProjectKey, Store,
};
use ai_memory_web::{
    HtmlAuthRedirectConfig, Recall, WebMountSpec, html_auth_redirect_mw, split_web_routers,
};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

type Scope = (WorkspaceId, ProjectId);

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

fn post(uri: &str, body: &Value) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .header("x-ai-memory-ui", "1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

struct World {
    store: Store,
    wiki: Wiki,
    a: Scope,
    b: Scope,
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

fn key(at: Scope) -> ProjectKey {
    ProjectKey {
        workspace_id: at.0,
        project_id: at.1,
    }
}

fn root(world: &World) -> axum::Router {
    app(&world.store, &world.wiki, AuthLevel::Root)
}

async fn mail(
    world: &World,
    from: Scope,
    from_agent: AgentKind,
    to: Scope,
    to_agent: AgentKind,
    subject: &str,
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
            subject: Some(subject.into()),
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

async fn tick() {
    tokio::time::sleep(Duration::from_millis(3)).await;
}

async fn page_and_fragment(world: &World, project: &str) -> (String, String) {
    let (status, page) = send(
        root(world),
        get(&format!("/web/chat?workspace=default&project={project}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (status, body) = send(
        root(world),
        get(&format!(
            "/web/chat/watch?workspace=default&project={project}&wait_ms=0"
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: Value = serde_json::from_str(&body).unwrap();
    (page, json["html"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn agent_bus_regression_hostile_mail_is_escaped_dropped_or_cut_on_the_chat_page() {
    let w = world().await;
    let hostile_html = "<script>alert('XSS-CANARY')</script><img src=x onerror=alert(1)>\
                        <a href=\"javascript:alert(2)\">click</a> \"quoted\" & <b>bold</b>";
    let hostile_marks =
        "start \u{202e}reversed\u{202c} \u{2066}isolate\u{2069} \u{200f}rlm \u{200b}zw end";
    let hostile_controls = "nul[\u{0}] esc[\u{1b}[31m] cr[\r] del[\u{7f}] bell[\u{7}] tab[\t] done";
    let long = format!("{}END-OF-LONG-BODY", "§".repeat(2_600));
    for (n, body) in [hostile_html, hostile_marks, hostile_controls, long.as_str()]
        .into_iter()
        .enumerate()
    {
        // The subject is text an agent wrote too.
        let subject = format!("<i>subject {n}</i>\u{202e}");
        mail(
            &w,
            w.a,
            AgentKind::ClaudeCode,
            w.a,
            AgentKind::Codex,
            &subject,
            body,
        )
        .await;
        tick().await;
    }
    let (page, fragment) = page_and_fragment(&w, "api").await;
    for (name, html) in [("page", &page), ("fragment", &fragment)] {
        // Markup the bodies carried is text, never elements or attributes.
        for raw in [
            "<script>alert('XSS-CANARY')",
            "<img src=x",
            "<a href=\"javascript",
            "<b>bold</b>",
            "<i>subject",
        ] {
            assert!(!html.contains(raw), "{name} holds raw markup {raw:?}");
        }
        assert!(
            html.contains("&lt;script&gt;alert(&#x27;XSS-CANARY&#x27;)&lt;/script&gt;"),
            "{name}"
        );
        assert!(html.contains("&lt;b&gt;bold&lt;/b&gt;"), "{name}");
        // Bidirectional marks, zero-width and control characters are dropped.
        for bad in [
            '\u{202e}', '\u{202c}', '\u{2066}', '\u{2069}', '\u{200f}', '\u{200b}', '\u{0}',
            '\u{1b}', '\u{7f}', '\u{7}', '\r',
        ] {
            assert!(!html.contains(bad), "{name} holds {bad:?}");
        }
        assert!(
            html.contains("isolate") && html.contains("end"),
            "{name}: the words stay"
        );
        // Text longer than 2000 characters is cut, whichever way it is read.
        assert!(
            !html.contains("END-OF-LONG-BODY"),
            "{name}: a long body was not cut"
        );
        let shown = html.matches('§').count();
        assert!(
            (1_990..=2_000).contains(&shown),
            "{name}: {shown} characters of a 2600-character body"
        );
    }
}

#[tokio::test]
async fn agent_bus_regression_another_projects_mail_never_shows_its_text_without_a_link() {
    let w = world().await;
    mail(
        &w,
        w.b,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        "SUBJECT-CANARY",
        "BODY-CANARY from the web project",
    )
    .await;
    let (page, fragment) = page_and_fragment(&w, "api").await;
    for (name, html) in [("page", &page), ("fragment", &fragment)] {
        assert!(
            !html.contains("BODY-CANARY"),
            "{name} shows another project's text"
        );
        assert!(
            !html.contains("SUBJECT-CANARY"),
            "{name} shows another project's subject"
        );
        assert!(
            html.contains("default/web"),
            "{name}: who and where stay visible"
        );
    }
    assert!(page.contains("data-cross=\"1\""));
}

#[tokio::test]
async fn agent_bus_regression_text_shows_for_mail_sent_during_a_link_and_not_after_it_ends() {
    let w = world().await;
    let ask = |reason: &str| NewLinkRequest {
        from: key(w.a),
        to: key(w.b),
        reason: reason.into(),
        valid_secs: 7_200,
        by: LinkRequester::Human,
        confirm_now: true,
    };
    mail(
        &w,
        w.b,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        "s",
        "BEFORE-LINK-TEXT",
    )
    .await;
    tick().await;
    let LinkRequestOutcome::Created(id, _) =
        w.store.writer.link_request(ask("share")).await.unwrap()
    else {
        panic!("expected a new link");
    };
    tick().await;
    mail(
        &w,
        w.b,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        "s",
        "DURING-LINK-TEXT",
    )
    .await;
    tick().await;
    // The page of either side shows the text of mail sent during the link.
    for project in ["api", "web"] {
        let (page, fragment) = page_and_fragment(&w, project).await;
        for html in [&page, &fragment] {
            assert!(html.contains("DURING-LINK-TEXT"), "{project}: {html}");
            assert!(
                !html.contains("BEFORE-LINK-TEXT"),
                "{project}: mail from before"
            );
        }
    }
    w.store
        .writer
        .link_close(id, LinkCloser::Human)
        .await
        .unwrap();
    tick().await;
    mail(
        &w,
        w.b,
        AgentKind::Codex,
        w.a,
        AgentKind::ClaudeCode,
        "s",
        "AFTER-LINK-TEXT",
    )
    .await;
    let (page, _) = page_and_fragment(&w, "api").await;
    assert!(
        page.contains("DURING-LINK-TEXT"),
        "the conversation of an ended link stays readable"
    );
    assert!(
        !page.contains("AFTER-LINK-TEXT"),
        "mail after the link is hidden again"
    );
    assert!(!page.contains("BEFORE-LINK-TEXT"));
}

#[tokio::test]
async fn agent_bus_regression_only_the_destinations_page_confirms_a_link() {
    let w = world().await;
    let (status, answer) = send(
        root(&w),
        post(
            "/web/chat/links",
            &json!({
                "from_workspace": "default", "from_project": "api",
                "to_workspace": "default", "to_project": "web",
                "reason": "share the schema", "hours": 2,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(answer["state"], "requested", "a request opens nothing");
    let id = answer["id"].as_str().unwrap().to_owned();
    let linked = || async {
        w.store
            .reader
            .projects_linked_at(w.a.1, w.b.1, jiff::Timestamp::now().as_microsecond())
            .await
            .unwrap()
    };
    let confirm = |project: &str| {
        post(
            &format!("/web/chat/links/{id}/confirm"),
            &json!({"workspace": "default", "project": project}),
        )
    };
    // The requester's page: 409, and still not linked.
    let (status, refused) = send(root(&w), confirm("api")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(refused.contains("not_destination"), "{refused}");
    assert!(!linked().await);
    // Its Confirm button is on the destination's page only.
    let (_, from_page) = send(
        root(&w),
        get("/web/chat?tab=cross&workspace=default&project=api"),
    )
    .await;
    // The waiting card is on every page, but its button names the
    // destination, never the requester, and the requester's table row has none.
    assert!(
        !from_page.contains("data-dest-project=\"api\""),
        "the requester can confirm"
    );
    assert_eq!(
        from_page
            .matches(&format!("data-link-confirm=\"{id}\""))
            .count(),
        1,
        "only the card, not the requester's table row, offers Confirm"
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

    let (status, done) = send(root(&w), confirm("web")).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert!(linked().await);
    let (status, _) = send(root(&w), confirm("web")).await;
    assert_eq!(status, StatusCode::CONFLICT, "confirming twice");

    // Either page can end it.
    let (status, _) = send(
        root(&w),
        post(&format!("/web/chat/links/{id}/close"), &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!linked().await);
}

#[tokio::test]
async fn agent_bus_regression_the_chat_is_closed_to_anyone_who_is_not_the_root_user() {
    let w = world().await;
    mail(
        &w,
        w.a,
        AgentKind::ClaudeCode,
        w.a,
        AgentKind::Codex,
        "s",
        "SECRET-TEXT",
    )
    .await;
    w.store
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
    let user = || app(&w.store, &w.wiki, AuthLevel::User);
    for request in [
        get("/web/chat?workspace=default&project=api"),
        get("/web/chat/watch?workspace=default&project=api&wait_ms=0"),
        get("/web/chat/pending-links"),
        get("/web/agent-bus/summary"),
        post(
            "/web/chat/links",
            &json!({"from_workspace": "default", "from_project": "api",
                    "to_workspace": "default", "to_project": "web", "reason": "x"}),
        ),
    ] {
        let (status, body) = send(user(), request).await;
        assert_ne!(status, StatusCode::OK, "{body}");
        assert!(!body.contains("SECRET-TEXT"), "{body}");
    }
}

/// Files a request from `api` to `web` through the page's own route and
/// returns its id.
async fn request_link(w: &World, reason: &str) -> String {
    let (status, answer) = send(
        root(w),
        post(
            "/web/chat/links",
            &json!({
                "from_workspace": "default", "from_project": "api",
                "to_workspace": "default", "to_project": "web",
                "reason": reason, "hours": 2,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let answer: Value = serde_json::from_str(&answer).unwrap();
    answer["id"].as_str().unwrap().to_owned()
}

async fn summary(w: &World) -> Value {
    let (status, body) = send(root(w), get("/web/agent-bus/summary")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn agent_bus_regression_a_waiting_request_is_found_from_any_project_and_any_tab() {
    let w = world().await;
    let ops = w
        .store
        .writer
        .get_or_create_project(w.a.0, "ops".to_string(), None)
        .await
        .unwrap();
    assert_eq!(
        summary(&w).await["pending_links"],
        0,
        "no request, no badge"
    );
    let (_, quiet) = send(
        root(&w),
        get("/web/chat?tab=cross&workspace=default&project=ops"),
    )
    .await;
    assert!(
        !quiet.contains("data-pending-link=\"1\""),
        "no card without a request"
    );

    let id = request_link(
        &w,
        "share the schema <script>alert('REASON-CANARY')</script>",
    )
    .await;

    // The sidebar's counter reads the same count.
    assert_eq!(summary(&w).await["pending_links"], 1);

    // The card shows on every tab and for a project that is neither side, the
    // case that hid a request when the selector held the wrong project.
    for uri in [
        "/web/chat?workspace=default&project=ops",
        "/web/chat?tab=cross&workspace=default&project=ops",
        "/web/chat?workspace=default&project=api",
        "/web/chat?tab=cross&workspace=default&project=web",
    ] {
        let (status, page) = send(root(&w), get(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(
            page.matches("data-pending-link=\"1\"").count(),
            1,
            "one card, not one per place, on {uri}"
        );
        assert!(page.contains("default / api → default / web"), "{uri}");
        // The button names the destination, whatever the selector holds.
        assert!(
            page.contains(&format!(
                "data-link-confirm=\"{id}\" data-dest-workspace=\"default\" data-dest-project=\"web\""
            )),
            "{uri}"
        );
        assert!(page.contains(&format!("data-link-close=\"{id}\"")), "{uri}");
        assert!(
            page.contains("data-waiting=\"1\""),
            "the tab counts it on {uri}"
        );
        // The reason is a quotation, escaped, never markup.
        assert!(page.contains("class=\"chat-quote\""), "{uri}");
        assert!(
            !page.contains("<script>alert('REASON-CANARY')"),
            "the reason became markup on {uri}"
        );
        assert!(page.contains("&lt;script&gt;"), "{uri}");
    }
    let _ = ops;

    // The refresh fragment carries the same card and its count.
    let (status, body) = send(root(&w), get("/web/chat/pending-links")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["count"], 1);
    let html = body["html"].as_str().unwrap();
    assert!(html.contains(&format!("data-link-request=\"{id}\"")));
    assert!(!html.contains("<script>"), "{html}");

    // Looking at it opened nothing and confirmed nothing.
    assert!(
        !w.store
            .reader
            .projects_linked_at(w.a.1, w.b.1, jiff::Timestamp::now().as_microsecond())
            .await
            .unwrap()
    );
    let links = w.store.reader.agent_links(None, false, 10).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].state.as_str(), "requested");
}

#[tokio::test]
async fn agent_bus_regression_a_card_confirms_only_for_the_destination_and_leaves_with_the_request()
{
    let w = world().await;
    let id = request_link(&w, "share the schema").await;
    let linked = || async {
        w.store
            .reader
            .projects_linked_at(w.a.1, w.b.1, jiff::Timestamp::now().as_microsecond())
            .await
            .unwrap()
    };
    // What a card sends is the destination's names; the requester's names
    // are refused whatever page the button is on.
    let (status, refused) = send(
        root(&w),
        post(
            &format!("/web/chat/links/{id}/confirm"),
            &json!({"workspace": "default", "project": "api"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(!linked().await);
    assert_eq!(summary(&w).await["pending_links"], 1);

    let (status, done) = send(
        root(&w),
        post(
            &format!("/web/chat/links/{id}/confirm"),
            &json!({"workspace": "default", "project": "web"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert!(linked().await);
    assert_eq!(
        summary(&w).await["pending_links"],
        0,
        "confirmed, no longer waiting"
    );
    let (_, page) = send(root(&w), get("/web/chat?workspace=default&project=ops")).await;
    assert!(
        !page.contains("data-pending-link=\"1\""),
        "no card once confirmed"
    );

    // A declined request leaves the cards too.
    let (status, _) = send(
        root(&w),
        post(&format!("/web/chat/links/{id}/close"), &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second = request_link(&w, "once more").await;
    assert_eq!(summary(&w).await["pending_links"], 1);
    let (status, _) = send(
        root(&w),
        post(&format!("/web/chat/links/{second}/close"), &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        summary(&w).await["pending_links"],
        0,
        "declined, not waiting"
    );
    let (_, body) = send(root(&w), get("/web/chat/pending-links")).await;
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["count"], 0);
    assert!(
        !body["html"]
            .as_str()
            .unwrap()
            .contains("data-pending-link=\"1\"")
    );
}

#[tokio::test]
async fn agent_bus_regression_the_sidebar_item_is_hidden_until_the_summary_says_there_is_a_request()
{
    let w = world().await;
    let (_, page) = send(root(&w), get("/web/chat?workspace=default&project=api")).await;
    assert!(
        page.contains("id=\"pending-links-nav\"")
            && page.contains("pending-links-nav\" data-nav-pending-links hidden"),
        "the item starts hidden, so a page without a request shows no empty badge"
    );
    assert!(page.contains("href=\"chat?tab=cross#pending-links\""));
    assert!(
        page.contains("summary.pending_links"),
        "the script fills it"
    );
}
