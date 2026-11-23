//! `GET /agent-bus`: root-only view of the agent bus. It lists the agents
//! hook sessions reported a wake handle for, with whether that session is
//! open and the newest wake outcome reported for each, the pairs of agents
//! the loop guard paused (with a button that resumes one), and the programs
//! the host supervisor trusted. It shows names, enums and times; no message
//! text, message id or session id.
//!
//! `POST /agent-bus/resume` resumes one paused pair. It takes JSON, the
//! `x-ai-memory-ui` header and a same-origin request, like the inbox
//! composer, and the root credential.

use std::sync::Arc;

use ai_memory_core::{AgentKind, AuthLevel, ProjectId, WorkspaceId};
use ai_memory_store::AgentSide;
use askama::Template;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;

use crate::state::WebState;
use crate::templates::{AgentBusView, BusPairRow, BusProgramRow, BusWakeRow};

/// Most rows each list on the page shows.
const PAGE_LIMIT: usize = 200;

fn when(micros: i64) -> String {
    jiff::Timestamp::from_microsecond(micros)
        .map(|at| at.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// Handler for `GET /agent-bus`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    let reader = &state.reader;
    let (handles, wakes, pauses, programs) = match (
        reader.wake_handles_overview(PAGE_LIMIT).await,
        reader.last_wakes(PAGE_LIMIT).await,
        reader.paused_agent_pairs(PAGE_LIMIT).await,
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
            tracing::error!("web agent bus read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let agents = handles
        .into_iter()
        .map(|(workspace, project, agent, handle)| {
            let last = wakes.iter().find(|wake| {
                wake.workspace == workspace && wake.project == project && wake.agent == agent
            });
            BusWakeRow {
                scope: format!("{workspace}/{project}"),
                agent: agent.as_str().to_owned(),
                kind: handle.kind.as_str().to_owned(),
                open: handle.open,
                last_seen: when(handle.last_seen_at),
                last_wake: last.map(|wake| match &wake.reason {
                    Some(reason) => format!("{}/{reason}", wake.event),
                    None => wake.event.clone(),
                }),
                last_wake_at: last.map(|wake| when(wake.at)).unwrap_or_default(),
            }
        })
        .collect();
    let pairs = pauses
        .iter()
        .map(|pause| {
            let (a, a_ws, a_pj) = &pause.a;
            let (b, b_ws, b_pj) = &pause.b;
            BusPairRow {
                a: format!("{} · {a_ws}/{a_pj}", a.agent.as_str()),
                b: format!("{} · {b_ws}/{b_pj}", b.agent.as_str()),
                reason: pause.reason.as_str().to_owned(),
                paused_at: when(pause.paused_at),
                trips: pause.trip_count,
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
    let programs = programs
        .iter()
        .filter_map(|event| {
            let detail: serde_json::Value = serde_json::from_str(&event.detail).ok()?;
            let text = |key: &str| detail[key].as_str().unwrap_or_default().to_owned();
            Some(BusProgramRow {
                at: when(event.at),
                adapter: text("adapter"),
                program: text("program"),
                sha256: text("sha256"),
                trusted: text("trusted"),
            })
        })
        .collect();
    let view = AgentBusView {
        agents,
        pairs,
        programs,
        i18n,
    };
    match view.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "web agent bus render failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Handler for `GET /agent-bus/summary`: the counts the sidebar shows beside
/// "Agent chat" (mail between agents not yet popped by its recipient) and
/// "Agent bus" (paused pairs), and the "Pending links" item (link requests
/// that wait for their destination's confirmation). Root only; anyone else gets the refusal and
/// the sidebar keeps both items hidden.
pub(crate) async fn summary(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    match (
        state.reader.pending_agent_mail_count().await,
        state.reader.paused_agent_pairs(PAGE_LIMIT).await,
        state.reader.agent_links(None, false, PAGE_LIMIT).await,
    ) {
        (Ok(unread), Ok(pairs), Ok(links)) => Json(serde_json::json!({
            "chat_unread": unread,
            "paused_pairs": pairs.len(),
            "pending_links": links
                .iter()
                .filter(|link| link.state == ai_memory_core::agent_links::LinkState::Requested)
                .count(),
        }))
        .into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Body of `POST /agent-bus/resume`: the two sides of a paused pair, as the
/// page lists them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumePair {
    a_workspace_id: String,
    a_project_id: String,
    a_agent: String,
    b_workspace_id: String,
    b_project_id: String,
    b_agent: String,
}

fn side(workspace: &str, project: &str, agent: &str) -> Option<AgentSide> {
    let kind = AgentKind::from_wire(agent);
    (kind.as_str() == agent).then_some(())?;
    Some(AgentSide {
        workspace_id: workspace.parse::<WorkspaceId>().ok()?,
        project_id: project.parse::<ProjectId>().ok()?,
        agent: kind,
    })
}

/// Handler for `POST /agent-bus/resume`.
pub(crate) async fn resume(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(input): Json<ResumePair>,
) -> Response {
    if headers.get("x-ai-memory-ui").and_then(|h| h.to_str().ok()) != Some("1") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) {
        let authority = origin
            .parse::<axum::http::Uri>()
            .ok()
            .and_then(|u| u.authority().map(|a| a.as_str().to_owned()));
        if authority.as_deref() != headers.get("host").and_then(|h| h.to_str().ok()) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    let (Some(a), Some(b)) = (
        side(&input.a_workspace_id, &input.a_project_id, &input.a_agent),
        side(&input.b_workspace_id, &input.b_project_id, &input.b_agent),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match state.wiki.resume_agent_pair(a, b).await {
        Ok(resumed) => Json(serde_json::json!({ "resumed": resumed })).into_response(),
        Err(error) => {
            tracing::error!(%error, "web agent bus resume failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
