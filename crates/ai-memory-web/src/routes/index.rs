//! `GET /` — project list cards.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;

use crate::state::WebState;
use crate::templates::{
    DashboardStats, IngestStats, OkfDialog, ProjectCard, ProjectsView, humanize, project_href,
};

/// Handler for `GET /`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
) -> Result<Html<String>, StatusCode> {
    let summaries = state
        .reader
        .list_projects_with_stats()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let dashboard = DashboardStats {
        project_count: summaries.len(),
        page_count: summaries.iter().map(|s| s.page_count).sum(),
        session_count: summaries.iter().map(|s| s.session_count).sum(),
        observation_count: summaries.iter().map(|s| s.observation_count).sum(),
        open_session_count: summaries.iter().map(|s| s.open_session_count).sum(),
        last_activity_relative: summaries
            .iter()
            .find_map(|s| s.last_activity.as_deref())
            .map(humanize)
            .unwrap_or_default(),
        ingest: state.ingest_metrics.as_ref().map(|metrics| {
            let snapshot = metrics.snapshot();
            IngestStats {
                accepted: snapshot.accepted,
                dropped_by_policy: snapshot.dropped_by_policy,
                shed_saturated: snapshot.shed_saturated,
                shed_rate_limited: snapshot.shed_rate_limited,
            }
        }),
    };
    let projects = summaries
        .into_iter()
        .map(|s| {
            let last_updated_relative =
                s.last_activity.as_deref().map(humanize).unwrap_or_default();
            let href = project_href(&s.workspace_name, &s.project_name);
            ProjectCard {
                workspace: s.workspace_name,
                project: s.project_name,
                page_count: s.page_count,
                session_count: s.session_count,
                observation_count: s.observation_count,
                open_session_count: s.open_session_count,
                last_updated_relative,
                href,
            }
        })
        .collect();

    let okf_dialog = okf_dialog(&state);
    let html = ProjectsView {
        projects,
        dashboard,
        okf_dialog,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}

/// The one-time migration explainer: rendered whenever a receipt
/// exists — even after the archive was deleted, so the "what happened"
/// context stays reachable — and dismissed per browser client-side.
fn okf_dialog(state: &WebState) -> Option<OkfDialog> {
    let receipt = ai_memory_wiki::backup::BackupReceipt::load(state.wiki.data_dir())?;
    Some(OkfDialog {
        archive_present: receipt.archive_present(),
        archive_path: receipt.archive_path.display().to_string(),
        size_human: human_bytes(receipt.size_bytes),
        created_at: receipt.created_at,
    })
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
