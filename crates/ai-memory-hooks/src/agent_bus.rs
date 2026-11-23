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
//! (a session id the daemon passes to an adapter as one argv element, with
//! whether the session is open and the directory it runs in) and the message
//! ids. Never a subject, body, sender or evidence. The sender gate
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
/// ones whose newest mail is newest, so new mail always reaches the daemon.
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
    /// The session's working directory as its hooks reported it; a resumed
    /// Claude Code or Gemini CLI session must run there.
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
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
                cwd: target.handle.cwd,
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

/// Refuse a request that lacks the client marker or the root credential.
fn root_only(
    headers: &HeaderMap,
    level: Option<&Extension<AuthLevel>>,
    viewer: Option<&Extension<AuthorizedViewer>>,
) -> Option<Response> {
    if headers
        .get(crate::CLIENT_MARKER_HEADER)
        .is_none_or(|value| value.as_bytes().is_empty())
    {
        return Some(refusal(
            StatusCode::FORBIDDEN,
            format!(
                "the agent bus requires the {} header",
                crate::CLIENT_MARKER_HEADER
            ),
        ));
    }
    if viewer.is_some() || matches!(level, Some(Extension(AuthLevel::User))) {
        return Some(refusal(
            StatusCode::FORBIDDEN,
            "the agent bus lists every project and needs the root credential",
        ));
    }
    None
}

/// Most rows each list of `GET /inbox/bus/status` returns.
const STATUS_LIMIT: usize = 200;

fn timestamp(micros: i64) -> String {
    jiff::Timestamp::from_microsecond(micros)
        .map(|at| at.to_string())
        .unwrap_or_default()
}

/// `GET /inbox/bus/status`: what `ai-memory agent-bus status` and `doctor`
/// show. The discovered agents with their session state and newest wake
/// outcome, the paused pairs, and the programs the host supervisor trusted.
/// Names, enums and times only; no session id, message id or text.
pub(crate) async fn handle_bus_status(
    State(state): State<Arc<HookState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = root_only(&headers, level.as_ref(), viewer.as_ref()) {
        return refused;
    }
    let reader = &state.reader;
    let (handles, wakes, pauses, programs) = match (
        reader.wake_handles_overview(STATUS_LIMIT).await,
        reader.last_wakes(STATUS_LIMIT).await,
        reader.paused_agent_pairs(STATUS_LIMIT).await,
        reader
            .list_audit_events(ai_memory_store::AuditLogFilter {
                op: Some("agent_bus_program_trusted".to_owned()),
                limit: 20,
                ..ai_memory_store::AuditLogFilter::default()
            })
            .await,
    ) {
        (Ok(handles), Ok(wakes), Ok(pauses), Ok(programs)) => (handles, wakes, pauses, programs),
        _ => {
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the agent bus",
            );
        }
    };
    let agents: Vec<serde_json::Value> = handles
        .into_iter()
        .map(|(workspace, project, agent, handle)| {
            let last = wakes.iter().find(|wake| {
                wake.workspace == workspace && wake.project == project && wake.agent == agent
            });
            serde_json::json!({
                "workspace": workspace,
                "project": project,
                "agent": agent.as_str(),
                "handle_kind": handle.kind.as_str(),
                "open": handle.open,
                "last_seen_at": timestamp(handle.last_seen_at),
                "last_wake": last.map(|wake| serde_json::json!({
                    "event": wake.event,
                    "reason": wake.reason,
                    "at": timestamp(wake.at),
                })),
            })
        })
        .collect();
    let side = |(side, workspace, project): &(ai_memory_store::AgentSide, String, String)| {
        serde_json::json!({
            "workspace": workspace,
            "project": project,
            "agent": side.agent.as_str(),
        })
    };
    let paused: Vec<serde_json::Value> = pauses
        .iter()
        .map(|pause| {
            serde_json::json!({
                "a": side(&pause.a),
                "b": side(&pause.b),
                "reason": pause.reason.as_str(),
                "paused_at": timestamp(pause.paused_at),
                "trip_count": pause.trip_count,
            })
        })
        .collect();
    let programs: Vec<serde_json::Value> = programs
        .iter()
        .map(|event| {
            let detail: serde_json::Value = serde_json::from_str(&event.detail).unwrap_or_default();
            serde_json::json!({ "at": timestamp(event.at), "program": detail })
        })
        .collect();
    Json(serde_json::json!({
        "agents": agents,
        "paused_pairs": paused,
        "programs": programs,
    }))
    .into_response()
}

/// Body of `POST /inbox/bus/program`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BusProgramReport {
    adapter: String,
    program: String,
    sha256: String,
    #[serde(default)]
    previous_sha256: Option<String>,
    trusted: String,
}

fn is_sha256(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// `POST /inbox/bus/program`: the host supervisor reports that it trusted an
/// adapter's program on first use, or a changed binary at the recorded
/// path. The server audits it (`agent_bus_program_trusted`) so the dashboard
/// and the audit log show every binary the bus may run.
pub(crate) async fn handle_bus_program(
    State(state): State<Arc<HookState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    headers: HeaderMap,
    body: Result<Json<BusProgramReport>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(refused) = root_only(&headers, level.as_ref(), viewer.as_ref()) {
        return refused;
    }
    let Json(report) = match body {
        Ok(body) => body,
        Err(rejection) => return refusal(rejection.status(), rejection.body_text()),
    };
    let known = ai_memory_core::message_wake::AdapterKind::parse(&report.adapter).is_some();
    let valid = known
        && matches!(report.trusted.as_str(), "first_use" | "changed")
        && is_sha256(&report.sha256)
        && report.previous_sha256.as_deref().is_none_or(is_sha256)
        && report.program.starts_with('/')
        && report.program.len() <= 1_024
        && !report.program.chars().any(char::is_control);
    if !valid {
        return refusal(StatusCode::BAD_REQUEST, "not a program report");
    }
    let detail = serde_json::json!({
        "adapter": report.adapter,
        "program": report.program,
        "sha256": report.sha256,
        "previous_sha256": report.previous_sha256,
        "trusted": report.trusted,
    });
    match state.writer.record_bus_program(detail.to_string()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => {
            warn!(error = %failure, "agent bus program report failed");
            refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not record the report",
            )
        }
    }
}
