//! Non-consuming mailbox history and an authorized browser message composer.

use crate::{
    state::WebState,
    templates::{agent_label, encode_segment, humanize, project_href},
    views::{InboxView, MessageRow, RecipientOption, ScopeOption},
};
use ai_memory_core::{
    ActorContext, AgentKind, AuthLevel, Capability, MessageBox, MessageState, NewAgentMessage,
};
use ai_memory_store::{ProjectAccess, lookup_existing_scope_guarded};
use askama::Template;
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

#[derive(Default, Deserialize)]
pub(crate) struct InboxQuery {
    workspace: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default, rename = "box")]
    mailbox: Option<String>,
}

fn mailbox_href(workspace: &str, project: &str, outbox: bool, offset: usize) -> String {
    format!(
        "inbox?workspace={}&project={}&box={}&offset={offset}",
        encode_segment(workspace),
        encode_segment(project),
        if outbox { "outbox" } else { "inbox" }
    )
}

pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    legacy: Option<axum::Extension<ai_memory_core::LegacyBrowserAuth>>,
    Query(query): Query<InboxQuery>,
) -> Response {
    let viewer = viewer.map(|v| v.0.user());
    let projects = match state.reader.list_projects_with_stats(viewer).await {
        Ok(p) => p,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let requested = match (query.workspace, query.project) {
        (Some(w), Some(p)) => (w, p),
        (None, None) => projects
            .first()
            .map(|p| (p.workspace_name.clone(), p.project_name.clone()))
            .unwrap_or_default(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Select both workspace and project.",
            )
                .into_response();
        }
    };
    let (workspace, project) = requested;
    let mailbox = match query.mailbox.as_deref() {
        None | Some("inbox") => MessageBox::Inbox,
        Some("outbox") => MessageBox::Outbox,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let inbox = mailbox == MessageBox::Inbox;
    let scope = if workspace.is_empty() && project.is_empty() {
        None
    } else {
        match super::resolve_read_scope(&state, viewer, &workspace, &project).await {
            Ok(s) => Some(s),
            Err(e) => return super::page::refusal_response(&e),
        }
    };
    let directory = match state.reader.registered_agents(viewer).await {
        Ok(d) => d,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let recipients = directory
        .into_iter()
        .map(|r| {
            let selected = r.workspace == workspace
                && r.project == project
                && query.agent.as_deref() == Some(r.agent.as_str());
            RecipientOption {
                label: agent_label(r.agent).into(),
                workspace: r.workspace,
                project: r.project,
                agent: r.agent.as_str().into(),
                selected,
            }
        })
        .collect::<Vec<_>>();
    let visible: HashMap<_, _> = projects
        .iter()
        .map(|p| {
            (
                (p.workspace_id.clone(), p.project_id.clone()),
                format!("{} / {}", p.workspace_name, p.project_name),
            )
        })
        .collect();
    let mut messages = match scope {
        Some(scope) => match state
            .reader
            .message_history(
                scope.workspace_id,
                scope.project_id,
                mailbox,
                51,
                query.offset,
            )
            .await
        {
            Ok(m) => m,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => Vec::new(),
    };
    let more = messages.len() > 50;
    messages.truncate(50);
    let rows = messages
        .into_iter()
        .map(|m| {
            let from = visible
                .get(&(
                    m.origin.from_workspace_id.to_string(),
                    m.origin.from_project_id.to_string(),
                ))
                .cloned()
                .unwrap_or_else(|| "A project you cannot read".into());
            let to = visible
                .get(&(m.to_workspace_id.to_string(), m.to_project_id.to_string()))
                .cloned()
                .unwrap_or_else(|| "A project you cannot read".into());
            MessageRow {
                id: m.id.to_string(),
                subject: m
                    .subject
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "Untitled message".into()),
                body: m.body,
                from,
                to,
                agent: m
                    .to_agent
                    .map(|a| agent_label(a).to_owned())
                    .unwrap_or_else(|| "Any project agent".into()),
                state: m.state.as_str().into(),
                state_label: match m.state {
                    MessageState::Pending => "Queued",
                    MessageState::Claimed => "Collected",
                    MessageState::Cancelled => "Cancelled",
                },
                when: humanize(&m.created_at.to_string()),
            }
        })
        .collect();
    let view = InboxView {
        scopes: projects
            .into_iter()
            .map(|p| ScopeOption {
                selected: p.workspace_name == workspace && p.project_name == project,
                workspace: p.workspace_name,
                project: p.project_name,
            })
            .collect(),
        inbox_href: mailbox_href(&workspace, &project, false, 0),
        outbox_href: mailbox_href(&workspace, &project, true, 0),
        previous: if query.offset > 0 {
            mailbox_href(
                &workspace,
                &project,
                !inbox,
                query.offset.saturating_sub(50),
            )
        } else {
            String::new()
        },
        next: if more {
            mailbox_href(
                &workspace,
                &project,
                !inbox,
                query.offset.saturating_add(50),
            )
        } else {
            String::new()
        },
        can_send: recipients
            .iter()
            .any(|r| r.workspace == workspace && r.project == project),
        needs_write_token: legacy.is_some(),
        project_href: project_href(&workspace, &project),
        workspace,
        project,
        recipients,
        rows,
        inbox,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ComposeMessage {
    from_workspace: String,
    from_project: String,
    to_workspace: String,
    to_project: String,
    to_agent: String,
    subject: String,
    body: String,
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error":message}))).into_response()
}

pub(crate) async fn send(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ActorContext>>,
    level: Option<axum::Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(input): Json<ComposeMessage>,
) -> Response {
    // A custom header plus JSON prevents a cross-site form from using cached
    // Basic credentials. Cookie sessions additionally pass dual-auth CSRF.
    if headers.get("x-ai-memory-ui").and_then(|h| h.to_str().ok()) != Some("1") {
        return error(StatusCode::FORBIDDEN, "Refresh the inbox and try again.");
    }
    if let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) {
        let authority = origin
            .parse::<axum::http::Uri>()
            .ok()
            .and_then(|u| u.authority().map(|a| a.as_str().to_owned()));
        if authority.as_deref() != headers.get("host").and_then(|h| h.to_str().ok()) {
            return error(
                StatusCode::FORBIDDEN,
                "Messages must be sent from this dashboard.",
            );
        }
    }
    let distinguishes = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(v) => v,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let level = level.map_or(AuthLevel::Anonymous, |l| l.0);
    if distinguishes && level == AuthLevel::Anonymous {
        return error(StatusCode::UNAUTHORIZED, "Sign in to send a message.");
    }
    if level
        .authorize(Capability::NormalWrite, distinguishes)
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let viewer = viewer.map(|v| v.0.user());
    if level == AuthLevel::User && viewer.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let source = match lookup_existing_scope_guarded(
        &state.reader,
        &input.from_workspace,
        &input.from_project,
        viewer,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return super::page::refusal_response(&e),
    };
    let target = match lookup_existing_scope_guarded(
        &state.reader,
        &input.to_workspace,
        &input.to_project,
        viewer,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return super::page::refusal_response(&e),
    };
    let agent = AgentKind::from_wire(&input.to_agent);
    if agent.as_str() != input.to_agent {
        return error(StatusCode::BAD_REQUEST, "Select a registered agent.");
    }
    let known = match state
        .reader
        .agent_usage(
            target.workspace_id,
            target.project_id,
            ai_memory_core::OwnerFilter::Any,
        )
        .await
    {
        Ok(rows) => rows.iter().any(|r| r.agent == input.to_agent),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if !known {
        return error(
            StatusCode::BAD_REQUEST,
            "That agent has no registered session in the recipient project.",
        );
    }
    if input.body.trim().is_empty()
        || input.body.chars().count() > 8_000
        || input.subject.chars().count() > 200
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Enter a message up to 8,000 characters and a subject up to 200 characters.",
        );
    }
    let actor = actor.map_or_else(ActorContext::anonymous, |a| a.0);
    let message = NewAgentMessage {
        from_workspace_id: source.workspace_id,
        from_project_id: source.project_id,
        from_agent: AgentKind::Other,
        from_session_id: None,
        from_owner_user: ai_memory_core::owner_stamp(actor.identity_key().as_ref(), distinguishes),
        to_workspace_id: target.workspace_id,
        to_project_id: target.project_id,
        to_agent: Some(agent),
        subject: Some(input.subject.trim().to_owned()),
        body: input.body.trim().to_owned(),
        nature: ai_memory_core::MessageNature::Unknown,
        evidence: None,
    };
    match state.wiki.send_agent_message(message, actor).await {
        Ok(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"message_id":id.to_string(),"state":"pending"})),
        )
            .into_response(),
        Err(ai_memory_wiki::WikiError::Store(ai_memory_store::StoreError::InvalidState(_))) => {
            error(
                StatusCode::CONFLICT,
                "This inbox is full. An agent must collect pending mail first.",
            )
        }
        Err(e) => {
            tracing::warn!(error=%e,"browser message refused");
            error(
                StatusCode::FORBIDDEN,
                "The message could not be queued. Check project access and message admission policy.",
            )
        }
    }
}
