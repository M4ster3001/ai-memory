//! `GET /inbox/bus`: the agent bus daemon's long-poll over every inbox
//! (`docs/design-agent-bus.md`).
//!
//! `ai-memory message supervise` without configured entries asks which
//! agents have claimable mail addressed to them and a wake handle that hook
//! ingest recorded for one of their sessions. The server holds the request
//! until any inbox commits new mail or the wait runs out, like
//! `GET /inbox/watch` does for one inbox. One waiter covers the whole host, so
//! the daemon needs no configuration and takes one watcher slot.
//!
//! The answer names the recipient coordinate, its harness, the newest handle
//! (a session id the daemon passes to an adapter as one argv element) and the
//! message ids. Never a subject, body, sender or evidence. The sender gate
//! still runs on the server, per agent, through `POST /inbox/wake-candidates`.
//!
//! Every project is listed, so the route needs the root credential (or a
//! server without auth): a database user is refused with `403`.

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::{AuthLevel, AuthorizedViewer};
use ai_memory_store::MAX_PENDING_INBOX_MESSAGES;
use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::inbox_watch::MAX_INBOX_WATCH_WAIT_MS;
use crate::router::HookState;

/// Most agents one answer lists. A host with more waiting agents gets the
/// ones whose mail waited longest; the rest come in later answers.
pub const MAX_BUS_TARGETS: usize = 64;

/// Query of `GET /inbox/bus`. Unknown parameters are refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxBusQuery {
    after: Option<u64>,
    #[serde(default)]
    wait_ms: u64,
}

#[derive(Debug, Serialize)]
struct InboxBusResponse {
    cursor: String,
    agents: Vec<BusAgentView>,
}

#[derive(Debug, Serialize)]
struct BusAgentView {
    workspace: String,
    project: String,
    agent: String,
    handle: HandleView,
    pending: Vec<String>,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct HandleView {
    kind: &'static str,
    id: String,
    open: bool,
    last_seen_at: String,
}

fn refusal(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

/// `GET /inbox/bus`.
pub(crate) async fn handle_inbox_bus(
    State(state): State<Arc<HookState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    headers: HeaderMap,
    Query(query): Query<InboxBusQuery>,
) -> Response {
    if headers
        .get(crate::CLIENT_MARKER_HEADER)
        .is_none_or(|value| value.as_bytes().is_empty())
    {
        return refusal(
            StatusCode::FORBIDDEN,
            format!(
                "the agent bus requires the {} header",
                crate::CLIENT_MARKER_HEADER
            ),
        );
    }
    if viewer.is_some() || matches!(level, Some(Extension(AuthLevel::User))) {
        return refusal(
            StatusCode::FORBIDDEN,
            "the agent bus lists every project and needs the root credential",
        );
    }
    if query.wait_ms > MAX_INBOX_WATCH_WAIT_MS {
        return refusal(
            StatusCode::BAD_REQUEST,
            format!("wait_ms must be at most {MAX_INBOX_WATCH_WAIT_MS}"),
        );
    }
    let Some(mut watch) = state.writer.inbox_signals().try_watch_all() else {
        let mut response = refusal(
            StatusCode::TOO_MANY_REQUESTS,
            "too many inbox watchers; retry shortly",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    // As in `GET /inbox/watch`: the cursor is taken before the read it
    // describes, so a commit after it is seen by the next request.
    let mut cursor = watch.generation();
    if query.wait_ms > 0 && query.after == Some(cursor) {
        tokio::select! {
            _ = watch.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(query.wait_ms)) => {}
        }
        cursor = watch.generation();
    }
    let max_ids = usize::try_from(MAX_PENDING_INBOX_MESSAGES).unwrap_or(usize::MAX);
    let targets = match state.reader.bus_targets(MAX_BUS_TARGETS, max_ids).await {
        Ok(targets) => targets,
        Err(failure) => {
            warn!(error = %failure, "agent bus read failed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the inboxes",
            );
        }
    };
    let agents = targets
        .into_iter()
        .map(|target| BusAgentView {
            workspace: target.workspace,
            project: target.project,
            agent: target.agent.as_str().to_owned(),
            handle: HandleView {
                kind: target.handle.kind.as_str(),
                id: target.handle.handle,
                open: target.handle.open,
                last_seen_at: jiff::Timestamp::from_microsecond(target.handle.last_seen_at)
                    .map(|at| at.to_string())
                    .unwrap_or_default(),
            },
            pending: target.pending.iter().map(ToString::to_string).collect(),
            truncated: target.truncated,
        })
        .collect();
    Json(InboxBusResponse {
        cursor: cursor.to_string(),
        agents,
    })
    .into_response()
}
