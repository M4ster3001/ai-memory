//! Authenticated, build-matched product help. No project data is read here.

use crate::{
    documentation::{self, Document},
    markdown,
    templates::VERSION,
};
use askama::Template;
use axum::{
    Json,
    extract::{Path, Query},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

#[derive(Default, Deserialize)]
pub(crate) struct DocsQuery {
    #[serde(default)]
    q: String,
    release: Option<String>,
}

#[derive(Template)]
#[template(path = "documentation_index.html")]
struct IndexView {
    query: String,
    docs: Vec<&'static Document>,
}

struct ReleaseOption {
    id: String,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "documentation_page.html")]
struct DocumentView {
    doc: &'static Document,
    html: String,
    outline: Vec<markdown::OutlineEntry>,
    current_href: String,
    markdown_href: String,
    releases: Vec<ReleaseOption>,
    selected_release: String,
    sidebar: &'static [Document],
}

fn response(body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-cache"),
    );
    response
}

pub(crate) async fn index(Query(query): Query<DocsQuery>) -> Response {
    if query.q.chars().count() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let docs = documentation::search(&query.q);
    match (IndexView {
        query: query.q,
        docs,
    })
    .render()
    {
        Ok(html) => response(Html(html)),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn selected<'a>(
    doc: &'a Document,
    requested: Option<&str>,
) -> Result<(&'a str, String, Vec<ReleaseOption>), StatusCode> {
    let releases = if doc.id.starts_with("changelog") {
        documentation::releases(doc.body)
    } else {
        Vec::new()
    };
    if releases.is_empty() {
        return if requested.is_some() {
            Err(StatusCode::BAD_REQUEST)
        } else {
            Ok((doc.body, String::new(), Vec::new()))
        };
    }
    let id = requested.unwrap_or(releases[0].id);
    let body = if id == "all" {
        doc.body
    } else {
        releases
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.body)
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let mut options: Vec<_> = releases
        .iter()
        .map(|r| ReleaseOption {
            id: r.id.into(),
            label: r.heading.trim_start_matches("## ").into(),
            selected: r.id == id,
        })
        .collect();
    options.push(ReleaseOption {
        id: "all".into(),
        label: "All entries in this series".into(),
        selected: id == "all",
    });
    Ok((body, id.into(), options))
}

pub(crate) async fn page(Path(slug): Path<String>, Query(query): Query<DocsQuery>) -> Response {
    let raw = slug.ends_with(".md");
    let Some(doc) = documentation::find(slug.strip_suffix(".md").unwrap_or(&slug)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (body, selected_release, releases) = match selected(doc, query.release.as_deref()) {
        Ok(s) => s,
        Err(s) => return s.into_response(),
    };
    if raw {
        return response((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            body,
        ));
    }
    let suffix = if selected_release.is_empty() {
        String::new()
    } else {
        format!(
            "?release={}",
            crate::templates::encode_segment(&selected_release)
        )
    };
    let current_href = format!("docs/{}{suffix}", doc.id);
    let rendered = markdown::render_reference_document(body, |url| {
        documentation::link(doc, &current_href, url)
    });
    let view = DocumentView {
        doc,
        html: rendered.html,
        outline: rendered.outline,
        markdown_href: format!("docs/{}.md{suffix}", doc.id),
        current_href,
        releases,
        selected_release,
        sidebar: documentation::DOCUMENTS,
    };
    match view.render() {
        Ok(html) => response(Html(html)),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(crate) async fn api_index(Query(query): Query<DocsQuery>) -> Response {
    if query.q.chars().count() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    response(Json(
        serde_json::json!({"version":VERSION,"documents":documentation::search(&query.q)}),
    ))
}

pub(crate) async fn api_page(Path(slug): Path<String>, Query(query): Query<DocsQuery>) -> Response {
    let Some(doc) = documentation::find(&slug) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (body, release, options) = match selected(doc, query.release.as_deref()) {
        Ok(s) => s,
        Err(s) => return s.into_response(),
    };
    response(Json(
        serde_json::json!({"version":VERSION,"document":doc,"markdown":body,"release":release,"releases":options.into_iter().map(|r|r.id).collect::<Vec<_>>()}),
    ))
}
