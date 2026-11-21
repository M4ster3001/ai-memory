//! `GET /chat`: root-only chat between the agents of one project, and the
//! cross-project tab (`docs/agent-bus/CROSS_PROJECT.md`).
//!
//! The page merges addressed mail between agents and room posts into one
//! timeline, with the delivery state of each mail, receipts folded under it,
//! and a banner with a Resume button for a paused pair. Text written by
//! agents is untrusted data: every string reaches the browser through the
//! template's escaping (never as HTML), control and bidirectional characters
//! are removed, and long bodies are cut. Another project's text is shown only
//! for mail sent while a link joined the two projects (the store decides, in
//! `ChatScope`).
//!
//! `GET /chat/watch` is a long-poll that answers with the fresh timeline when
//! something in the project changed. It waits on a cheap fingerprint query, so
//! it never calls a model, and at most [`MAX_WATCHERS`] run at once.
//!
//! The `POST` routes act as the root user: they take JSON, the
//! `x-ai-memory-ui` header and a same-origin request, like the inbox
//! composer, and refuse a database user.

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::agent_links::{LinkState, link_reason, link_validity};
use ai_memory_core::rooms::{MAX_ROOM_POST_CHARS, RoomName};
use ai_memory_core::{AgentKind, AgentLinkId, AuthLevel};
use ai_memory_store::{
    ChatItem, ChatMail, ChatPost, ChatScope, LinkConfirmOutcome, LinkRequestOutcome, LinkView,
    NewLinkRequest, ProjectAccess, ProjectKey, lookup_existing_scope_guarded,
};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use crate::state::WebState;
use crate::templates::{
    ChatBubble, ChatFeed, ChatFeedView, ChatLinkRow, ChatPause, ChatProjectOption, ChatRequest,
    ChatView, encode_segment,
};

/// Most long-poll requests the page holds open at once.
const MAX_WATCHERS: usize = 16;
/// Longest wait one long-poll request may ask for.
const MAX_WAIT_MS: u64 = 25_000;
/// How often a waiting request looks at the fingerprint.
const POLL_EVERY: Duration = Duration::from_millis(900);
/// Characters of one body or reason the page shows.
const SHOWN_CHARS: usize = 2_000;
/// Most projects the selector lists.
const PROJECT_LIMIT: usize = 500;
/// Most paused pairs and links read for a page.
const PAGE_LIMIT: usize = 200;

static WATCHERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_WATCHERS);

fn when(micros: i64) -> String {
    jiff::Timestamp::from_microsecond(micros)
        .map(|at| at.strftime("%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

fn when_full(micros: i64) -> String {
    jiff::Timestamp::from_microsecond(micros)
        .map(|at| at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_default()
}

/// Text an agent wrote, as the page may show it: no control characters except
/// line breaks and tabs, no bidirectional overrides, at most `limit`
/// characters. Returns the text and whether it was cut.
pub(crate) fn shown_text(raw: &str, limit: usize) -> (String, bool) {
    let mut out = String::new();
    let mut count = 0;
    for c in raw.chars() {
        let bad = (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if bad {
            continue;
        }
        if count == limit {
            return (out, true);
        }
        out.push(c);
        count += 1;
    }
    (out, false)
}

fn tone(agent: AgentKind) -> (&'static str, String) {
    match agent {
        AgentKind::ClaudeCode => ("claude", "CC".to_owned()),
        AgentKind::Codex => ("codex", "CX".to_owned()),
        AgentKind::GeminiCli => ("gemini", "GM".to_owned()),
        AgentKind::Other => ("human", "YOU".to_owned()),
        other => (
            "other",
            other
                .as_str()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(2)
                .collect::<String>()
                .to_uppercase(),
        ),
    }
}

fn sender_label(agent: AgentKind, i18n: &ai_memory_core::i18n::Localizer) -> String {
    if agent == AgentKind::Other {
        i18n.text("web-chat-human")
    } else {
        agent.as_str().to_owned()
    }
}

fn delivery_tone(state: &str) -> &'static str {
    match state {
        "delivered" => "ok",
        "wake_failed" | "agent_unavailable" | "quota_exhausted" | "expired" | "dead"
        | "cancelled" => "bad",
        "queued" | "awaiting_agent" | "wake_attempted" => "wait",
        _ => "plain",
    }
}

fn mail_bubble(
    mail: ChatMail,
    shown: ai_memory_store::ProjectKey,
    i18n: &ai_memory_core::i18n::Localizer,
) -> ChatBubble {
    let (tone, badge) = tone(mail.from_agent);
    let cross = mail.scope != ChatScope::Internal;
    let other = if mail.from.key.project_id == shown.project_id {
        &mail.to
    } else {
        &mail.from
    };
    let to_agent = mail.to_agent.map_or_else(
        || i18n.text("web-chat-any-agent"),
        |a| a.as_str().to_owned(),
    );
    let direction = if mail.from.key.project_id == shown.project_id {
        format!(
            "→ {}{}",
            to_agent,
            if cross {
                format!(" · {}/{}", mail.to.workspace, mail.to.project)
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "{}/{} → {}",
            mail.from.workspace, mail.from.project, to_agent
        )
    };
    let (body, truncated) = mail
        .body
        .as_deref()
        .map_or((String::new(), false), |body| shown_text(body, SHOWN_CHARS));
    let subject = mail
        .subject
        .as_deref()
        .map(|subject| shown_text(subject, 200).0)
        .unwrap_or_default();
    let delivery = match &mail.delivery_reason {
        Some(reason) => format!("{}/{reason}", mail.delivery_state),
        None => mail.delivery_state.clone(),
    };
    // The mail a room mention creates repeats the post it points at: show it
    // as a note under the post, with its delivery state, not as a bubble.
    let mention = mail.scope == ChatScope::Internal
        && mail.subject.as_deref().is_some_and(|subject| {
            subject.starts_with("room ") && subject.ends_with(": you were mentioned")
        })
        && mail
            .body
            .as_deref()
            .is_some_and(|body| body.starts_with("You were mentioned in room \""));
    ChatBubble {
        kind: if mention { "mention" } else { "mail" },
        key: mail.id.to_string(),
        side: if mail.from_agent == AgentKind::Other {
            "end"
        } else {
            "start"
        },
        tone,
        badge,
        sender: sender_label(mail.from_agent, i18n),
        route: direction,
        other_project: if cross {
            format!("{}/{}", other.workspace, other.project)
        } else {
            String::new()
        },
        cross,
        redacted: mail.scope == ChatScope::Hidden,
        subject: if mention { String::new() } else { subject },
        body: if mention { String::new() } else { body },
        truncated: truncated && !mention,
        when: when(mail.at),
        when_full: when_full(mail.at),
        delivery_tone: delivery_tone(&mail.delivery_state),
        delivery,
        read: mail.state == "claimed",
        receipts: mail
            .receipts
            .iter()
            .map(|receipt| match &receipt.reason {
                Some(reason) => format!("{}/{reason}", receipt.outcome),
                None => receipt.outcome.clone(),
            })
            .collect(),
        mentions: String::new(),
    }
}

fn post_bubble(post: ChatPost, i18n: &ai_memory_core::i18n::Localizer) -> ChatBubble {
    let (tone, badge) = tone(post.from_agent);
    let (body, truncated) = shown_text(&post.body, SHOWN_CHARS);
    ChatBubble {
        kind: "post",
        key: format!("{}#{}", post.room, post.seq),
        side: if post.from_agent == AgentKind::Other {
            "end"
        } else {
            "start"
        },
        tone,
        badge,
        sender: sender_label(post.from_agent, i18n),
        route: format!("#{}", post.room),
        other_project: String::new(),
        cross: false,
        redacted: false,
        subject: String::new(),
        body,
        truncated,
        when: when(post.at),
        when_full: when_full(post.at),
        delivery: String::new(),
        delivery_tone: "plain",
        read: false,
        receipts: Vec::new(),
        mentions: post
            .mentions
            .iter()
            .map(|agent| agent.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn asked_by(link: &LinkView) -> String {
    match (link.requested_by_human, link.requested_agent) {
        (true, _) => "human".to_owned(),
        (false, Some(agent)) => format!("agent · {}", agent.as_str()),
        (false, None) => "agent".to_owned(),
    }
}

fn hours(secs: i64) -> String {
    let value = secs as f64 / 3600.0;
    if (value - value.round()).abs() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// The live part of the page for one project.
async fn build_feed(
    state: &WebState,
    project: ProjectKey,
    i18n: &ai_memory_core::i18n::Localizer,
) -> Result<ChatFeed, ()> {
    let reader = &state.reader;
    let (items, pauses, links) = match tokio::try_join!(
        reader.chat_timeline(project, ai_memory_store::CHAT_ITEM_LIMIT),
        reader.paused_agent_pairs(PAGE_LIMIT),
        reader.agent_links(Some(project), false, PAGE_LIMIT),
    ) {
        Ok(read) => read,
        Err(error) => {
            tracing::error!(%error, "web chat read failed");
            return Err(());
        }
    };
    let items = items
        .into_iter()
        .map(|item| match item {
            ChatItem::Mail(mail) => mail_bubble(mail, project, i18n),
            ChatItem::Post(post) => post_bubble(post, i18n),
        })
        .collect();
    let pauses = pauses
        .iter()
        .filter(|pause| {
            pause.a.0.project_id == project.project_id || pause.b.0.project_id == project.project_id
        })
        .map(|pause| {
            let (a, a_ws, a_pj) = &pause.a;
            let (b, b_ws, b_pj) = &pause.b;
            ChatPause {
                a: format!("{} · {a_ws}/{a_pj}", a.agent.as_str()),
                b: format!("{} · {b_ws}/{b_pj}", b.agent.as_str()),
                reason: pause.reason.as_str().to_owned(),
                paused_at: when(pause.paused_at),
                key: serde_json::json!({
                    "a_workspace_id": a.workspace_id.to_string(),
                    "a_project_id": a.project_id.to_string(),
                    "a_agent": a.agent.as_str(),
                    "b_workspace_id": b.workspace_id.to_string(),
                    "b_project_id": b.project_id.to_string(),
                    "b_agent": b.agent.as_str(),
                })
                .to_string(),
            }
        })
        .collect();
    let requests = links
        .iter()
        .filter(|link| link.state == LinkState::Requested && link.to.key == project)
        .map(|link| ChatRequest {
            id: link.id.to_string(),
            from: format!("{} / {}", link.from.workspace, link.from.project),
            reason: shown_text(&link.reason, 300).0,
            asked_by: asked_by(link),
            valid_hours: hours(link.valid_secs),
            expires: when(link.expires_at),
        })
        .collect();
    Ok(ChatFeed {
        items,
        pauses,
        requests,
    })
}

/// Query of `GET /chat`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ChatQuery {
    workspace: Option<String>,
    project: Option<String>,
    tab: Option<String>,
}

/// Handler for `GET /chat`.
pub(crate) async fn handler(
    Extension(i18n): Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    Query(query): Query<ChatQuery>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    let tab = match query.tab.as_deref() {
        None | Some("" | "chat") => "chat",
        Some("cross") => "cross",
        Some(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let listed = match state.reader.list_projects_with_stats(None).await {
        Ok(projects) => projects,
        Err(error) => {
            tracing::error!(%error, "web chat project list failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut listed: Vec<_> = listed.into_iter().take(PROJECT_LIMIT).collect();
    listed.sort_by(|a, b| {
        (&a.workspace_name, &a.project_name).cmp(&(&b.workspace_name, &b.project_name))
    });
    let chosen = match (query.workspace.as_deref(), query.project.as_deref()) {
        (Some(workspace), Some(project)) => {
            let found = listed
                .iter()
                .find(|p| p.workspace_name == workspace && p.project_name == project);
            match found {
                Some(found) => Some(found.clone()),
                None => return StatusCode::NOT_FOUND.into_response(),
            }
        }
        (None, None) => {
            // The project that last had an agent report in, else the first.
            let newest = state
                .reader
                .wake_handles_overview(1)
                .await
                .ok()
                .and_then(|mut rows| {
                    rows.pop()
                        .map(|(workspace, project, _, _)| (workspace, project))
                });
            newest
                .and_then(|(workspace, project)| {
                    listed
                        .iter()
                        .find(|p| p.workspace_name == workspace && p.project_name == project)
                        .cloned()
                })
                .or_else(|| listed.first().cloned())
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let options = |selected: Option<&ai_memory_store::ProjectSummary>| -> Vec<ChatProjectOption> {
        listed
            .iter()
            .map(|p| ChatProjectOption {
                workspace: p.workspace_name.clone(),
                project: p.project_name.clone(),
                selected: selected.is_some_and(|s| {
                    s.workspace_name == p.workspace_name && s.project_name == p.project_name
                }),
            })
            .collect()
    };
    let Some(chosen) = chosen else {
        let view = ChatView {
            i18n,
            tab,
            workspace: String::new(),
            project: String::new(),
            projects: Vec::new(),
            chat_href: "chat".to_owned(),
            cross_href: "chat?tab=cross".to_owned(),
            cursor: String::new(),
            feed: ChatFeed {
                items: Vec::new(),
                pauses: Vec::new(),
                requests: Vec::new(),
            },
            rooms: Vec::new(),
            links: Vec::new(),
            waiting_count: 0,
            has_projects: false,
        };
        return render(&view);
    };
    let (Ok(workspace_id), Ok(project_id)) = (
        chosen.workspace_id.parse::<ai_memory_core::WorkspaceId>(),
        chosen.project_id.parse::<ai_memory_core::ProjectId>(),
    ) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let key = ProjectKey {
        workspace_id,
        project_id,
    };
    let Ok(feed) = build_feed(&state, key, &i18n).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let (cursor, rooms, links) = match tokio::try_join!(
        state.reader.chat_fingerprint(key),
        state.reader.rooms(workspace_id, project_id),
        state.reader.agent_links(Some(key), true, 100),
    ) {
        Ok(read) => read,
        Err(error) => {
            tracing::error!(%error, "web chat read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let waiting_count = feed.requests.len();
    let links = links
        .iter()
        .map(|link| {
            let destination = link.to.key == key;
            ChatLinkRow {
                id: link.id.to_string(),
                from: format!("{}/{}", link.from.workspace, link.from.project),
                to: format!("{}/{}", link.to.workspace, link.to.project),
                state: link.state.as_str().to_owned(),
                reason: shown_text(&link.reason, 300).0,
                asked_by: asked_by(link),
                ends: if link.state.is_live() {
                    when(link.expires_at)
                } else {
                    link.closed_at.map(when).unwrap_or_default()
                },
                confirmed_via: link.confirmed_via.clone().unwrap_or_default(),
                can_confirm: link.state == LinkState::Requested && destination,
                can_close: link.state.is_live(),
                destination_href: if link.state == LinkState::Requested && !destination {
                    format!(
                        "chat?tab=cross&workspace={}&project={}",
                        encode_segment(&link.to.workspace),
                        encode_segment(&link.to.project)
                    )
                } else {
                    String::new()
                },
            }
        })
        .collect();
    let query_string = format!(
        "workspace={}&project={}",
        encode_segment(&chosen.workspace_name),
        encode_segment(&chosen.project_name)
    );
    let view = ChatView {
        i18n,
        tab,
        projects: options(Some(&chosen)),
        chat_href: format!("chat?{query_string}"),
        cross_href: format!("chat?tab=cross&{query_string}"),
        workspace: chosen.workspace_name,
        project: chosen.project_name,
        cursor,
        feed,
        rooms: rooms.into_iter().map(|room| room.name).collect(),
        links,
        waiting_count,
        has_projects: true,
    };
    render(&view)
}

fn render<T: Template>(view: &T) -> Response {
    match view.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "web chat render failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Query of `GET /chat/watch`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WatchQuery {
    workspace: String,
    project: String,
    #[serde(default)]
    after: String,
    #[serde(default)]
    wait_ms: u64,
}

async fn scope(
    state: &WebState,
    workspace: &str,
    project: &str,
    need: ProjectAccess,
) -> Result<ProjectKey, Response> {
    lookup_existing_scope_guarded(&state.reader, workspace, project, None, need)
        .await
        .map(|scope| ProjectKey {
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
        })
        .map_err(|error| super::page::refusal_response(&error))
}

/// Handler for `GET /chat/watch`: the long-poll. It answers at once when the
/// project's fingerprint differs from `after` (or `wait_ms` is 0), else holds
/// the request until it does or the wait ends. `html` is the fresh live part
/// of the page, or `null` when nothing changed.
pub(crate) async fn watch(
    Extension(i18n): Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    Query(query): Query<WatchQuery>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    if query.wait_ms > MAX_WAIT_MS {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let key = match scope(
        &state,
        &query.workspace,
        &query.project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(key) => key,
        Err(response) => return response,
    };
    let Ok(_permit) = WATCHERS.try_acquire() else {
        let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
        response
            .headers_mut()
            .insert("retry-after", axum::http::HeaderValue::from_static("1"));
        return response;
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(query.wait_ms);
    let cursor = loop {
        let cursor = match state.reader.chat_fingerprint(key).await {
            Ok(cursor) => cursor,
            Err(error) => {
                tracing::error!(%error, "web chat fingerprint failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        if query.wait_ms == 0
            || cursor != query.after
            || tokio::time::Instant::now() + POLL_EVERY >= deadline
        {
            break cursor;
        }
        tokio::time::sleep(POLL_EVERY).await;
    };
    let html = if cursor == query.after && query.wait_ms > 0 {
        None
    } else {
        let Ok(feed) = build_feed(&state, key, &i18n).await else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        match (ChatFeedView { i18n, feed }).render() {
            Ok(html) => Some(html),
            Err(error) => {
                tracing::error!(%error, "web chat render failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };
    let mut response = Json(serde_json::json!({ "cursor": cursor, "html": html })).into_response();
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/// The checks every `POST` here shares: the UI header, a same-origin request
/// and the root credential.
async fn authorize_post(
    i18n: &ai_memory_core::i18n::Localizer,
    state: &WebState,
    level: Option<Extension<AuthLevel>>,
    headers: &HeaderMap,
) -> Result<(), Response> {
    if headers.get("x-ai-memory-ui").and_then(|h| h.to_str().ok()) != Some("1") {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    if let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) {
        let authority = origin
            .parse::<axum::http::Uri>()
            .ok()
            .and_then(|u| u.authority().map(|a| a.as_str().to_owned()));
        if authority.as_deref() != headers.get("host").and_then(|h| h.to_str().ok()) {
            return Err(StatusCode::FORBIDDEN.into_response());
        }
    }
    super::require_admin(i18n, state, level).await
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// Body of `POST /chat/post`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PostBody {
    workspace: String,
    project: String,
    room: String,
    body: String,
}

/// Handler for `POST /chat/post`: the root user writes to a room as
/// `human`. A mention becomes mail as for any post, and the post resumes the
/// project's paused pairs.
pub(crate) async fn post(
    Extension(i18n): Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(input): Json<PostBody>,
) -> Response {
    if let Err(response) = authorize_post(&i18n, &state, level, &headers).await {
        return response;
    }
    let Ok(room) = RoomName::parse(&input.room) else {
        return error(
            StatusCode::BAD_REQUEST,
            "A room name is 1 to 40 lowercase letters, digits or '-'.",
        );
    };
    if input.body.trim().is_empty() || input.body.chars().count() > MAX_ROOM_POST_CHARS {
        return error(StatusCode::BAD_REQUEST, "Write 1 to 4,000 characters.");
    }
    let key = match scope(
        &state,
        &input.workspace,
        &input.project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(key) => key,
        Err(response) => return response,
    };
    match state
        .wiki
        .post_human_room_message(key.workspace_id, key.project_id, room, input.body.trim())
        .await
    {
        Ok(outcome) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "seq": outcome.seq,
                "mentioned": outcome.mentioned.iter().map(|(agent, _)| agent.as_str()).collect::<Vec<_>>(),
                "mentions_dropped": outcome.mentions_dropped.iter().map(|agent| agent.as_str()).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(ai_memory_wiki::WikiError::Store(ai_memory_store::StoreError::InvalidState(_))) => {
            error(StatusCode::CONFLICT, "The room or the inbox is full.")
        }
        Err(error) => {
            tracing::warn!(%error, "web chat post failed");
            self::error(StatusCode::INTERNAL_SERVER_ERROR, "The post could not be saved.")
        }
    }
}

/// Body of `POST /chat/links`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkBody {
    from_workspace: String,
    from_project: String,
    to_workspace: String,
    to_project: String,
    reason: String,
    #[serde(default)]
    hours: Option<f64>,
    #[serde(default)]
    confirm_now: bool,
}

/// Handler for `POST /chat/links`: the root user asks to connect two
/// projects. Without `confirm_now` it only files the request; the
/// destination's page (or agent) confirms.
pub(crate) async fn create_link(
    Extension(i18n): Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(input): Json<LinkBody>,
) -> Response {
    if let Err(response) = authorize_post(&i18n, &state, level, &headers).await {
        return response;
    }
    let valid_secs = match input.hours {
        None => None,
        Some(hours) if hours.is_finite() && hours > 0.0 => Some((hours * 3600.0).round() as u64),
        Some(_) => return error(StatusCode::BAD_REQUEST, "Choose how long the link lasts."),
    };
    let (valid_secs, reason) = match (link_validity(valid_secs), link_reason(&input.reason)) {
        (Ok(secs), Ok(reason)) => (secs, reason),
        (Err(e), _) | (_, Err(e)) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let from = match scope(
        &state,
        &input.from_workspace,
        &input.from_project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(key) => key,
        Err(response) => return response,
    };
    let to = match scope(
        &state,
        &input.to_workspace,
        &input.to_project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(key) => key,
        Err(response) => return response,
    };
    let outcome = state
        .wiki
        .request_project_link(NewLinkRequest {
            from,
            to,
            reason,
            valid_secs,
            by: ai_memory_store::LinkRequester::Human,
            confirm_now: input.confirm_now,
        })
        .await;
    match outcome {
        Ok(LinkRequestOutcome::Created(id, link_state)) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"id": id.to_string(), "state": link_state.as_str()})),
        )
            .into_response(),
        Ok(LinkRequestOutcome::Existing(id, link_state)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": id.to_string(),
                "state": link_state.as_str(),
                "existing": true,
            })),
        )
            .into_response(),
        Ok(LinkRequestOutcome::Refused(refusal)) => {
            error(StatusCode::CONFLICT, refusal.explanation())
        }
        Err(failure) => {
            tracing::warn!(error = %failure, "web link request failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The request could not be saved.",
            )
        }
    }
}

fn link_id(raw: &str) -> Result<AgentLinkId, Response> {
    raw.parse::<AgentLinkId>()
        .map_err(|_| error(StatusCode::BAD_REQUEST, "That is not a link id."))
}

/// Body of `POST /chat/links/{id}/confirm`: the project whose page the root
/// user pressed Confirm on. The store accepts it only for the link's
/// destination.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfirmBody {
    workspace: String,
    project: String,
}

/// Handler for `POST /chat/links/{id}/confirm`.
pub(crate) async fn confirm_link(
    Extension(i18n): Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<ConfirmBody>,
) -> Response {
    if let Err(response) = authorize_post(&i18n, &state, level, &headers).await {
        return response;
    }
    let id = match link_id(&id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let viewing = match scope(
        &state,
        &input.workspace,
        &input.project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(key) => key,
        Err(response) => return response,
    };
    match state.wiki.confirm_project_link(id, viewing).await {
        Ok(LinkConfirmOutcome::Confirmed { .. }) => {
            Json(serde_json::json!({"confirmed": true})).into_response()
        }
        Ok(LinkConfirmOutcome::Refused(refusal)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": refusal.explanation(), "code": refusal.code()})),
        )
            .into_response(),
        Err(failure) => {
            tracing::warn!(error = %failure, "web link confirm failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The link could not be confirmed.",
            )
        }
    }
}

/// Handler for `POST /chat/links/{id}/close`: end a link, or decline a
/// request.
pub(crate) async fn close_link(
    Extension(i18n): Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = authorize_post(&i18n, &state, level, &headers).await {
        return response;
    }
    let id = match link_id(&id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match state.wiki.close_project_link(id).await {
        Ok(ai_memory_store::LinkCloseOutcome::Closed(_)) => {
            Json(serde_json::json!({"closed": true})).into_response()
        }
        Ok(ai_memory_store::LinkCloseOutcome::NotFound) => {
            error(StatusCode::NOT_FOUND, "No such link.")
        }
        Ok(_) => error(StatusCode::CONFLICT, "This link is already over."),
        Err(failure) => {
            tracing::warn!(error = %failure, "web link close failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The link could not be closed.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::shown_text;

    #[test]
    fn shown_text_drops_control_and_bidi_characters_and_cuts_long_text() {
        let (text, cut) = shown_text("a\u{1b}[31mb\u{202e}c\nd\te\u{0}", 100);
        assert_eq!(text, "a[31mbc\nd\te");
        assert!(!cut);
        let (text, cut) = shown_text(&"é".repeat(10), 4);
        assert_eq!(text.chars().count(), 4);
        assert!(cut);
        assert_eq!(shown_text("abcd", 4), ("abcd".to_owned(), false));
    }
}
