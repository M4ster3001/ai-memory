//! `GET /w/:workspace/:project` — page tree + recent activity.

use std::collections::BTreeMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Html;

use crate::state::WebState;
use crate::templates::{
    Folder, PageRow, ProjectMemoryStats, ProjectView, humanize, page_href,
};

/// Handler for `GET /w/:workspace/:project`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Result<Html<String>, StatusCode> {
    let pages = state
        .reader
        .list_pages(&workspace, &project)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // The page tree alone is a poor explanation for projects where hooks are
    // working but compilation has not produced a page yet. Reuse the same
    // metadata-only aggregate behind the dashboard; raw observations remain
    // accessible only through scoped API detail routes.
    let summary = state
        .reader
        .list_projects_with_stats()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .find(|item| item.workspace_name == workspace && item.project_name == project);
    let stats = match summary {
        Some(item) => ProjectMemoryStats {
            page_count: item.page_count,
            session_count: item.session_count,
            observation_count: item.observation_count,
            open_session_count: item.open_session_count,
            last_activity_relative: item
                .last_activity
                .as_deref()
                .map(humanize)
                .unwrap_or_default(),
        },
        // A page listing may still be visible while aggregate metadata is
        // being refreshed. Preserve a truthful compiled-page count.
        None => ProjectMemoryStats {
            page_count: pages.len() as u64,
            session_count: 0,
            observation_count: 0,
            open_session_count: 0,
            last_activity_relative: String::new(),
        },
    };

    // Build sidebar folder trees (group by first path segment), split
    // into knowledge and machinery. A store accumulates far more
    // machinery pages (lint reports, session captures, monthly logs,
    // bundle indexes) than curated knowledge; listing them as peers
    // buried the concepts/decisions/rules a human actually opens this
    // UI for.
    let mut knowledge_map: BTreeMap<String, Vec<PageRow>> = BTreeMap::new();
    let mut system_map: BTreeMap<String, Vec<PageRow>> = BTreeMap::new();
    for p in &pages {
        let folder = p
            .path
            .split('/')
            .next()
            .and_then(|seg| {
                // Only treat it as a folder prefix if there's a slash in the path.
                if p.path.contains('/') {
                    Some(seg.to_owned())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "(root)".to_owned());
        let map = if is_system_page(&p.path) {
            &mut system_map
        } else {
            &mut knowledge_map
        };
        map.entry(folder).or_default().push(PageRow {
            path: p.path.clone(),
            href: page_href(&workspace, &project, &p.path),
            title: p.title.clone(),
            kind: p.kind.clone(),
            updated_relative: humanize(&p.updated_at),
        });
    }
    let folders: Vec<Folder> = knowledge_map
        .into_iter()
        .map(|(name, pages)| Folder { name, pages })
        .collect();
    let system: Vec<Folder> = system_map
        .into_iter()
        .map(|(name, pages)| Folder { name, pages })
        .collect();

    // Recent pages: knowledge only, sorted by updated_at desc, take 20.
    // Machinery updates constantly (logs append every consolidation,
    // lint reruns daily), so an unfiltered recency sort would show
    // nothing else.
    let mut sorted: Vec<_> = pages
        .iter()
        .filter(|p| !is_system_page(&p.path))
        .cloned()
        .collect();
    sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sorted.truncate(20);
    let recent: Vec<PageRow> = sorted
        .into_iter()
        .map(|p| PageRow {
            path: p.path.clone(),
            href: page_href(&workspace, &project, &p.path),
            title: p.title.clone(),
            kind: p.kind.clone(),
            updated_relative: humanize(&p.updated_at),
        })
        .collect();

    let html = ProjectView {
        workspace,
        project,
        stats,
        folders,
        system,
        recent,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}

/// Machinery rather than knowledge: hidden from Recent Activity and
/// collapsed into the sidebar's System section. Underscore-prefixed
/// trees are system surfaces — except `_rules`, which holds standing
/// human-authored rules — as are session captures and the root-level
/// bookkeeping pages (monthly logs, the OKF bundle index, `_meta.md`).
fn is_system_page(path: &str) -> bool {
    if path.starts_with("_rules/") {
        return false;
    }
    if path.starts_with('_') || path.starts_with("sessions/") {
        return true;
    }
    if path.contains('/') {
        return false;
    }
    path == "index.md" || path == "_meta.md" || (path.starts_with("log-") && path.ends_with(".md"))
}
