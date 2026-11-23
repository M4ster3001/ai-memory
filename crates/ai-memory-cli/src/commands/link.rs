//! `ai-memory link`: links between two projects, through the server's
//! `/admin/links*` routes. Every subcommand prints the server's JSON answer,
//! so an agent that cannot use MCP reads it the way it would read the
//! `memory_link_*` tools.
//!
//! There is no way to confirm as the root user from here: that is the web
//! page's Confirm button. `link confirm` applies every rule an agent's
//! confirmation has, including the recorded session.

use anyhow::Result;
use serde::Serialize;

use crate::cli::{
    LinkArgs, LinkCloseArgs, LinkCommand, LinkConfirmArgs, LinkListArgs, LinkRequestArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

/// Run `ai-memory link`.
///
/// # Errors
/// An unresolvable scope or a refused request.
pub async fn run(config: &Config, args: LinkArgs) -> Result<()> {
    let answer = match args.command {
        LinkCommand::Request(args) => request(config, args).await?,
        LinkCommand::Confirm(args) => confirm(config, args).await?,
        LinkCommand::Close(args) => close(config, args).await?,
        LinkCommand::List(args) => list(config, args).await?,
    };
    println!("{}", serde_json::to_string_pretty(&answer)?);
    Ok(())
}

fn scope(
    config: &Config,
    workspace: Option<&str>,
    project: Option<&str>,
) -> Result<(String, String)> {
    super::refuse_empty_explicit(workspace, "--workspace")?;
    super::refuse_empty_explicit(project, "--project")?;
    super::resolve_scope(config, workspace, project)
}

/// The hours a request asks for, in seconds.
fn valid_secs(hours: Option<f64>) -> Result<Option<u64>> {
    match hours {
        None => Ok(None),
        Some(hours) if hours.is_finite() && hours > 0.0 => {
            Ok(Some((hours * 3600.0).round() as u64))
        }
        Some(_) => anyhow::bail!("--hours must be a positive number up to 24"),
    }
}

#[derive(Serialize)]
struct RequestBody<'a> {
    from_workspace: &'a str,
    from_project: &'a str,
    to_workspace: &'a str,
    to_project: &'a str,
    reason: &'a str,
    agent: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_secs: Option<u64>,
}

async fn request(config: &Config, args: LinkRequestArgs) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let valid_secs = valid_secs(args.hours)?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    post_json(
        &endpoint,
        "/admin/links/request",
        &RequestBody {
            from_workspace: &workspace,
            from_project: &project,
            to_workspace: &args.to_workspace,
            to_project: &args.to_project,
            reason: &args.reason,
            agent: &args.agent,
            session: args.session.as_deref(),
            valid_secs,
        },
    )
    .await
}

#[derive(Serialize)]
struct ConfirmBody<'a> {
    id: &'a str,
    workspace: &'a str,
    project: &'a str,
    agent: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
}

async fn confirm(config: &Config, args: LinkConfirmArgs) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    post_json(
        &endpoint,
        "/admin/links/confirm",
        &ConfirmBody {
            id: &args.link_id,
            workspace: &workspace,
            project: &project,
            agent: &args.agent,
            session: args.session.as_deref(),
        },
    )
    .await
}

#[derive(Serialize)]
struct CloseBody<'a> {
    id: &'a str,
}

async fn close(config: &Config, args: LinkCloseArgs) -> Result<serde_json::Value> {
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    post_json(
        &endpoint,
        "/admin/links/close",
        &CloseBody { id: &args.link_id },
    )
    .await
}

async fn list(config: &Config, args: LinkListArgs) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let mut query = vec![
        ("workspace", workspace.as_str()),
        ("project", project.as_str()),
    ];
    if let Some(agent) = args.agent.as_deref() {
        query.push(("agent", agent));
    }
    if let Some(session) = args.session.as_deref() {
        query.push(("session", session));
    }
    if args.all {
        query.push(("all", "true"));
    }
    get_json(&endpoint, "/admin/links", &query).await
}

#[cfg(test)]
mod tests {
    use super::valid_secs;

    #[test]
    fn hours_become_seconds_and_nonsense_is_refused() {
        assert_eq!(valid_secs(None).unwrap(), None);
        assert_eq!(valid_secs(Some(2.0)).unwrap(), Some(7_200));
        assert_eq!(valid_secs(Some(0.5)).unwrap(), Some(1_800));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(valid_secs(Some(bad)).is_err(), "{bad}");
        }
    }
}
