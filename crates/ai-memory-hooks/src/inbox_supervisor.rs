//! Routes for the host wake supervisor (`ai-memory message supervise`).
//!
//! The supervisor waits on `GET /inbox/watch` and then needs two things the
//! watch answer deliberately leaves out:
//!
//! - `POST /inbox/wake-candidates` applies its sender gate on the server. The
//!   supervisor sends the projects it may be woken by (`wake_from`); the
//!   answer says, per pending message, only whether the sender is another
//!   agent of the watched project, the watched agent itself, one of the
//!   listed projects with a recorded session, one of them without one,
//!   someone else, the server (a receipt), or a sender the loop guard paused
//!   with this agent. Sender names and ids never leave the server.
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
/// The labels are checked in order: receipt, own project, `wake_from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WakeSender {
    /// The watched project itself, from a harness other than the watched
    /// agent. Mail inside one project needs no `wake_from` and no recorded
    /// session.
    SameProject,
    /// The watched project itself, from the watched agent's own harness: an
    /// agent's mail to itself, which must not wake it (a wake that makes it
    /// write to itself again would loop). Two sessions of one harness in one
    /// project cannot be told apart, so their mail to each other lands here.
    SameAgent,
    /// Another project joined to the watched one by an active link
    /// (`agent_links`: requested, confirmed by the destination, not yet
    /// expired or closed). It wakes like `same_project`, and only for the
    /// two projects the link names.
    Linked,
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
    /// The loop guard paused wakes between this sender and the watched
    /// agent (`agent_pair_pauses`): the mail stays in the inbox, but it must
    /// not wake. A supervisor that predates the label treats it as unknown,
    /// which never wakes either.
    Paused,
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
            let own_project = candidate.from_workspace_id == scope.workspace_id
                && candidate.from_project_id == scope.project_id;
            let sender = match candidate.kind {
                MessageKind::Receipt => WakeSender::System,
                MessageKind::Message if own_project && candidate.from_agent == agent => {
                    WakeSender::SameAgent
                }
                MessageKind::Message if candidate.pair_paused => WakeSender::Paused,
                MessageKind::Message if own_project => WakeSender::SameProject,
                MessageKind::Message if candidate.linked => WakeSender::Linked,
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

/// The pending mail that may interrupt `agent` in the middle of a turn, from
/// its wake candidates in `(ws, proj)`: a receipt, or a message from another
/// harness of the same project or of a project joined to it by an active
/// link, that the loop guard did not pause. Mail from any other project, the
/// agent's own mail and paused mail never interrupt it, the same rule the
/// wake supervisor applies to a discovered agent.
#[must_use]
pub fn turn_notice_ids(
    candidates: &[ai_memory_store::WakeCandidate],
    ws: WorkspaceId,
    proj: ProjectId,
    agent: AgentKind,
) -> Vec<MessageId> {
    candidates
        .iter()
        .filter(|candidate| match candidate.kind {
            MessageKind::Receipt => true,
            MessageKind::Message => {
                let own = candidate.from_workspace_id == ws
                    && candidate.from_project_id == proj
                    && candidate.from_agent != agent;
                (own || candidate.linked) && !candidate.pair_paused
            }
        })
        .map(|candidate| candidate.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        kind: MessageKind,
        from: (WorkspaceId, ProjectId),
        agent: AgentKind,
        paused: bool,
    ) -> ai_memory_store::WakeCandidate {
        ai_memory_store::WakeCandidate {
            id: MessageId::new(),
            kind,
            from_workspace_id: from.0,
            from_project_id: from.1,
            from_agent: agent,
            sender_session_recorded: false,
            pair_paused: paused,
            linked: false,
        }
    }

    #[test]
    fn only_another_agent_of_the_project_or_a_receipt_interrupts_a_turn() {
        let here = (WorkspaceId::new(), ProjectId::new());
        let elsewhere = (here.0, ProjectId::new());
        let codex = AgentKind::Codex;
        let all = [
            candidate(MessageKind::Message, here, AgentKind::ClaudeCode, false),
            candidate(MessageKind::Message, here, codex, false),
            candidate(MessageKind::Message, here, AgentKind::GeminiCli, true),
            candidate(
                MessageKind::Message,
                elsewhere,
                AgentKind::ClaudeCode,
                false,
            ),
            candidate(MessageKind::Receipt, elsewhere, AgentKind::Other, false),
        ];
        let ids = turn_notice_ids(&all, here.0, here.1, codex);
        assert_eq!(ids, [all[0].id, all[4].id]);
    }

    #[test]
    fn mail_from_a_linked_project_interrupts_like_mail_from_the_own_project() {
        let here = (WorkspaceId::new(), ProjectId::new());
        let elsewhere = (here.0, ProjectId::new());
        let codex = AgentKind::Codex;
        let linked = |agent, paused| ai_memory_store::WakeCandidate {
            linked: true,
            ..candidate(MessageKind::Message, elsewhere, agent, paused)
        };
        let all = [
            linked(AgentKind::ClaudeCode, false),
            linked(codex, false),
            linked(AgentKind::GeminiCli, true),
            candidate(MessageKind::Message, elsewhere, AgentKind::Grok, false),
        ];
        let ids = turn_notice_ids(&all, here.0, here.1, codex);
        assert_eq!(
            ids,
            [all[0].id, all[1].id],
            "a linked project's agent of the same harness is another agent; only paused and \
             unlinked mail stays quiet"
        );
    }
}
