//! `GET /rooms`: root-only view of the agent bus. It lists every room with
//! its members and newest post number, and the agents hook sessions reported
//! a wake handle for, with whether that session is open. It only reads; post
//! text and session ids are not shown.

use std::sync::Arc;

use ai_memory_core::AuthLevel;
use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::state::WebState;
use crate::templates::{BusAgentRow, RoomRow, RoomsView};

/// Most rooms and agents the page lists.
const PAGE_LIMIT: usize = 200;

fn when(micros: i64) -> String {
    jiff::Timestamp::from_microsecond(micros)
        .map(|at| at.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// Handler for `GET /rooms`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    let rooms = match state.reader.rooms_overview(PAGE_LIMIT).await {
        Ok(rooms) => rooms,
        Err(error) => {
            tracing::error!(%error, "web rooms read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let agents = match state.reader.wake_handles_overview(PAGE_LIMIT).await {
        Ok(agents) => agents,
        Err(error) => {
            tracing::error!(%error, "web wake handles read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let view = RoomsView {
        rooms: rooms
            .into_iter()
            .map(|overview| RoomRow {
                scope: format!("{}/{}", overview.workspace, overview.project),
                name: overview.room.name,
                members: overview
                    .room
                    .members
                    .iter()
                    .map(|member| member.agent.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                last_seq: overview.room.last_seq,
            })
            .collect(),
        agents: agents
            .into_iter()
            .map(|(workspace, project, agent, handle)| BusAgentRow {
                scope: format!("{workspace}/{project}"),
                agent: agent.as_str().to_owned(),
                kind: handle.kind.as_str().to_owned(),
                open: handle.open,
                last_seen: when(handle.last_seen_at),
            })
            .collect(),
        i18n,
    };
    match view.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "web rooms render failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
