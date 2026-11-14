//! `GET /inbox/watch`: a model-free long-poll for new agent mail.
//!
//! A watcher (`ai-memory message watch`) asks whether the mail a harness can
//! claim in one inbox changed since the cursor it last saw, and the server
//! holds the request until the writer signals a commit or the wait runs out.
//! The answer carries message ids, a shared flag and timestamps only: never a
//! subject, body, evidence or sender, so nothing another project wrote can
//! reach a process that merely waits for mail. Reading a message stays a
//! deliberate `memory_message_pop`.
//!
//! The route is mounted beside `/handoff`, under the same bearer middleware,
//! and authorizes the inbox with a no-create lookup at `Read`, so a DB-user key
//! can watch the projects it can read without the root bearer. Waiters are
//! capped by [`ai_memory_store::MAX_INBOX_WATCHERS`]; a request past the cap is
//! answered `429` at once.

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::AgentKind;
use ai_memory_store::{
    MAX_PENDING_INBOX_MESSAGES, ProjectAccess, ScopeResolutionError, lookup_existing_scope_guarded,
};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::router::HookState;

/// Longest a single request may wait for a commit signal.
pub const MAX_INBOX_WATCH_WAIT_MS: u64 = 25_000;

/// Same bound the managed-workstream routes put on a workspace or project
/// name; the lookup that follows fails closed on anything it does not know.
const MAX_NAME_BYTES: usize = 256;

/// Query of `GET /inbox/watch`. Unknown parameters are refused rather than
/// ignored, so a misspelled filter cannot silently widen what is watched.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxWatchQuery {
    workspace: String,
    project: String,
    agent: String,
    #[serde(default)]
    include_shared: bool,
    after: Option<u64>,
    #[serde(default)]
    wait_ms: u64,
}

#[derive(Debug, Serialize)]
struct InboxWatchResponse {
    cursor: String,
    pending_count: u64,
    messages: Vec<WatchedMessage>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct WatchedMessage {
    id: String,
    shared: bool,
    created_at: String,
}

fn refusal(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

/// Validate one scope name before it reaches the lookup. The error is the
/// message of a `400`.
fn scope_name<'a>(label: &str, raw: &'a str) -> Result<&'a str, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(format!("{label} is not a valid name"));
    }
    Ok(name)
}

/// The harness to watch, accepted only in its canonical wire spelling: the
/// same rule `memory_message_send` applies to `to_agent`, which is the value
/// stored on the row this filter compares against.
fn watched_agent(raw: &str) -> Option<AgentKind> {
    let agent = AgentKind::from_wire(raw);
    (agent.as_str() == raw).then_some(agent)
}

fn scope_refusal(failure: &ScopeResolutionError) -> Response {
    let status = if failure.is_forbidden() {
        StatusCode::FORBIDDEN
    } else if failure.is_not_found() {
        StatusCode::NOT_FOUND
    } else if failure.is_bad_request() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    refusal(status, failure.to_string())
}

/// `GET /inbox/watch`.
pub(crate) async fn handle_inbox_watch(
    State(state): State<Arc<HookState>>,
    viewer_ext: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    headers: HeaderMap,
    Query(query): Query<InboxWatchQuery>,
) -> Response {
    // A page cannot attach a custom header without a preflight, which this
    // route never answers; without it a stray `<img>` on a plain-HTTP LAN
    // address could hold watcher slots.
    if headers
        .get(crate::CLIENT_MARKER_HEADER)
        .is_none_or(|value| value.as_bytes().is_empty())
    {
        return refusal(
            StatusCode::FORBIDDEN,
            format!(
                "inbox watch requires the {} header",
                crate::CLIENT_MARKER_HEADER
            ),
        );
    }
    let names = scope_name("workspace", &query.workspace)
        .and_then(|workspace| Ok((workspace, scope_name("project", &query.project)?)));
    let (workspace, project) = match names {
        Ok(names) => names,
        Err(message) => return refusal(StatusCode::BAD_REQUEST, message),
    };
    let Some(agent) = watched_agent(&query.agent) else {
        return refusal(
            StatusCode::BAD_REQUEST,
            "unknown agent: name a harness such as codex, grok or claude-code",
        );
    };
    if query.wait_ms > MAX_INBOX_WATCH_WAIT_MS {
        return refusal(
            StatusCode::BAD_REQUEST,
            format!("wait_ms must be at most {MAX_INBOX_WATCH_WAIT_MS}"),
        );
    }
    let viewer = viewer_ext.map(|axum::Extension(viewer)| viewer.user());
    let scope = match lookup_existing_scope_guarded(
        &state.reader,
        workspace,
        project,
        viewer,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(scope) => scope,
        Err(failure) => return scope_refusal(&failure),
    };
    let Some(mut watch) = state.writer.inbox_signals().try_watch(
        scope.workspace_id,
        scope.project_id,
        agent,
        query.include_shared,
    ) else {
        let mut response = refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "too many inbox watchers; retry shortly",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    // The cursor is taken before the read it describes. A commit that lands
    // after it bumps the generation, so the client's next request (or this
    // wait) sees it; one that landed before is in the read below.
    let mut cursor = watch.generation();
    if query.wait_ms > 0 && query.after == Some(cursor) {
        tokio::select! {
            _ = watch.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(query.wait_ms)) => {}
        }
        cursor = watch.generation();
    }
    let limit = usize::try_from(MAX_PENDING_INBOX_MESSAGES).unwrap_or(usize::MAX);
    let snapshot = match state
        .reader
        .inbox_watch_snapshot(
            scope.workspace_id,
            scope.project_id,
            agent,
            query.include_shared,
            limit,
        )
        .await
    {
        Ok(snapshot) => snapshot,
        Err(failure) => {
            warn!(error = %failure, "inbox watch read failed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the inbox",
            );
        }
    };
    let returned = snapshot.messages.len() as u64;
    Json(InboxWatchResponse {
        cursor: cursor.to_string(),
        pending_count: snapshot.total,
        truncated: snapshot.total > returned,
        messages: snapshot
            .messages
            .into_iter()
            .map(|(id, shared, created_at)| WatchedMessage {
                id: id.to_string(),
                shared,
                created_at: created_at.to_string(),
            })
            .collect(),
    })
    .into_response()
}
