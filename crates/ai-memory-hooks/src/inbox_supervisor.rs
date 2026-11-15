//! Routes for the host wake supervisor (`ai-memory message supervise`).
//!
//! The supervisor waits on `GET /inbox/watch` and then needs two things the
//! watch answer deliberately leaves out:
//!
//! - `POST /inbox/wake-candidates` applies its sender gate on the server. The
//!   supervisor sends the projects it may be woken by (`wake_from`); the
//!   answer says, per pending message, only whether the sender is one of them
//!   with a recorded session, is one of them without one, is someone else, or
//!   is the server (a receipt). Sender names and ids never leave the server.
//! - `POST /inbox/wake-report` records what a wake did, from enumerated values
//!   only (`state`, `reason`, a bounded `retry_after_secs`), so the server can
//!   set the delivery state, write the timeline and send the sender its
//!   receipt.
//!
//! Both sit beside `/inbox/watch` under the same bearer middleware, require
//! the client marker header, refuse unknown fields, and authorize the watched
//! inbox with a no-create lookup at `Write`: waking an agent and changing what
//! its senders see acts on that project.

use std::collections::HashSet;
use std::sync::Arc;

use ai_memory_core::message_wake::WakeOutcome;
use ai_memory_core::{
    AgentKind, DeliveryReason, DeliveryState, MessageId, MessageKind, ProjectId, WorkspaceId,
};
use ai_memory_store::{
    MAX_PENDING_INBOX_MESSAGES, ProjectAccess, ResolvedScope, ScopeResolutionError,
    lookup_existing_scope_guarded,
};
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::router::HookState;

/// Most projects one supervised agent may be woken by.
pub const MAX_WAKE_FROM: usize = 32;
/// Most message ids one wake report may name: a full inbox.
const MAX_REPORT_IDS: usize = 256;
const MAX_NAME_BYTES: usize = 256;

/// One project, by name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeNames {
    workspace: String,
    project: String,
}

/// Body of `POST /inbox/wake-candidates`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WakeCandidatesRequest {
    workspace: String,
    project: String,
    agent: String,
    #[serde(default)]
    include_shared: bool,
    #[serde(default)]
    wake_from: Vec<ScopeNames>,
}

/// Who sent a pending message, as far as the sender gate is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WakeSender {
    /// A project in `wake_from`, and the server recorded the sender's
    /// session there when the message was sent.
    Listed,
    /// A project in `wake_from`, but with no recorded session: the sending
    /// project was named, not shown.
    Unverified,
    /// Any other project.
    Unlisted,
    /// The server itself: a delivery receipt.
    System,
}

#[derive(Debug, Serialize)]
struct WakeCandidateView {
    id: String,
    sender: WakeSender,
}

#[derive(Debug, Serialize)]
struct WakeCandidatesResponse {
    messages: Vec<WakeCandidateView>,
    /// `wake_from` entries that name no project this caller can read. They
    /// match nothing.
    unresolved_wake_from: usize,
}

/// Body of `POST /inbox/wake-report`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WakeReportRequest {
    workspace: String,
    project: String,
    agent: String,
    message_ids: Vec<String>,
    state: DeliveryState,
    #[serde(default)]
    reason: Option<DeliveryReason>,
    #[serde(default)]
    retry_after_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
struct WakeReportResponse {
    updated: usize,
    receipts: usize,
}

fn refusal(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

/// A trimmed, bounded name, or the message a `400` refusal carries.
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

/// The `403` refusal for a request without the client marker header, or
/// `None` when it carries one. A page cannot attach a custom header without a
/// preflight, which these routes never answer.
fn missing_marker(headers: &HeaderMap) -> Option<Response> {
    headers
        .get(crate::CLIENT_MARKER_HEADER)
        .is_none_or(|value| value.as_bytes().is_empty())
        .then(|| {
            refusal(
                StatusCode::FORBIDDEN,
                format!(
                    "the wake supervisor routes require the {} header",
                    crate::CLIENT_MARKER_HEADER
                ),
            )
        })
}

/// Valid names, a canonical harness name, and `Write` on the watched inbox.
async fn authorize_inbox(
    state: &HookState,
    viewer: Option<ai_memory_core::UserId>,
    workspace: &str,
    project: &str,
    agent: &str,
) -> Result<(ResolvedScope, AgentKind), Response> {
    let bad_name = |message: String| refusal(StatusCode::BAD_REQUEST, message);
    let workspace = scope_name("workspace", workspace).map_err(bad_name)?;
    let project = scope_name("project", project).map_err(bad_name)?;
    let kind = AgentKind::from_wire(agent);
    if kind.as_str() != agent {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            "unknown agent: name a harness such as codex, grok or claude-code",
        ));
    }
    let scope = lookup_existing_scope_guarded(
        &state.reader,
        workspace,
        project,
        viewer,
        ProjectAccess::Write,
    )
    .await
    .map_err(|failure| scope_refusal(&failure))?;
    Ok((scope, kind))
}

fn body_refusal(rejection: &JsonRejection) -> Response {
    refusal(rejection.status(), rejection.body_text())
}

/// `POST /inbox/wake-candidates`.
pub(crate) async fn handle_wake_candidates(
    State(state): State<Arc<HookState>>,
    viewer_ext: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    headers: HeaderMap,
    body: Result<Json<WakeCandidatesRequest>, JsonRejection>,
) -> Response {
    if let Some(response) = missing_marker(&headers) {
        return response;
    }
    let viewer = viewer_ext.map(|axum::Extension(viewer)| viewer.user());
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_refusal(&rejection),
    };
    let (scope, agent) = match authorize_inbox(
        &state,
        viewer,
        &request.workspace,
        &request.project,
        &request.agent,
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    if request.wake_from.len() > MAX_WAKE_FROM {
        return refusal(
            StatusCode::BAD_REQUEST,
            format!("wake_from may name at most {MAX_WAKE_FROM} projects"),
        );
    }
    // A listed project the caller cannot read matches nothing; resolving it
    // with the caller's own access keeps a gate from confirming the sender of
    // a message by naming a project the caller may not see.
    let mut listed: HashSet<(WorkspaceId, ProjectId)> = HashSet::new();
    let mut unresolved = 0;
    for names in &request.wake_from {
        let (Ok(workspace), Ok(project)) = (
            scope_name("wake_from workspace", &names.workspace),
            scope_name("wake_from project", &names.project),
        ) else {
            unresolved += 1;
            continue;
        };
        match lookup_existing_scope_guarded(
            &state.reader,
            workspace,
            project,
            viewer,
            ProjectAccess::Read,
        )
        .await
        {
            Ok(found) => {
                listed.insert((found.workspace_id, found.project_id));
            }
            Err(_) => unresolved += 1,
        }
    }
    let limit = usize::try_from(MAX_PENDING_INBOX_MESSAGES).unwrap_or(usize::MAX);
    let candidates = match state
        .reader
        .wake_candidates(
            scope.workspace_id,
            scope.project_id,
            agent,
            request.include_shared,
            limit,
        )
        .await
    {
        Ok(candidates) => candidates,
        Err(failure) => {
            warn!(error = %failure, "wake candidates read failed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the inbox",
            );
        }
    };
    let messages = candidates
        .into_iter()
        .map(|candidate| {
            let sender = match candidate.kind {
                MessageKind::Receipt => WakeSender::System,
                MessageKind::Message
                    if listed
                        .contains(&(candidate.from_workspace_id, candidate.from_project_id)) =>
                {
                    if candidate.sender_session_recorded {
                        WakeSender::Listed
                    } else {
                        WakeSender::Unverified
                    }
                }
                MessageKind::Message => WakeSender::Unlisted,
            };
            WakeCandidateView {
                id: candidate.id.to_string(),
                sender,
            }
        })
        .collect();
    Json(WakeCandidatesResponse {
        messages,
        unresolved_wake_from: unresolved,
    })
    .into_response()
}

/// `POST /inbox/wake-report`.
pub(crate) async fn handle_wake_report(
    State(state): State<Arc<HookState>>,
    viewer_ext: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    headers: HeaderMap,
    body: Result<Json<WakeReportRequest>, JsonRejection>,
) -> Response {
    if let Some(response) = missing_marker(&headers) {
        return response;
    }
    let viewer = viewer_ext.map(|axum::Extension(viewer)| viewer.user());
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_refusal(&rejection),
    };
    let (scope, agent) = match authorize_inbox(
        &state,
        viewer,
        &request.workspace,
        &request.project,
        &request.agent,
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let outcome =
        match WakeOutcome::from_report(request.state, request.reason, request.retry_after_secs) {
            Ok(outcome) => outcome,
            Err(error) => return refusal(StatusCode::BAD_REQUEST, error.to_string()),
        };
    if request.message_ids.is_empty() || request.message_ids.len() > MAX_REPORT_IDS {
        return refusal(
            StatusCode::BAD_REQUEST,
            format!("message_ids must name 1 to {MAX_REPORT_IDS} messages"),
        );
    }
    let mut ids = Vec::with_capacity(request.message_ids.len());
    for raw in &request.message_ids {
        match raw.parse::<MessageId>() {
            Ok(id) if id.to_string() == *raw => ids.push(id),
            _ => {
                return refusal(
                    StatusCode::BAD_REQUEST,
                    "message_ids must be canonical message ids",
                );
            }
        }
    }
    match state
        .writer
        .record_wake_report(scope.workspace_id, scope.project_id, agent, ids, outcome)
        .await
    {
        Ok(report) => Json(WakeReportResponse {
            updated: report.updated,
            receipts: report.receipts,
        })
        .into_response(),
        Err(failure) => {
            warn!(error = %failure, "wake report failed");
            refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not record the wake report",
            )
        }
    }
}
