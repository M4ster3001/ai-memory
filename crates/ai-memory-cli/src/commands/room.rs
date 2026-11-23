//! `ai-memory room`: rooms among the agents of one project, through the
//! server's `/admin/rooms*` routes. Every subcommand prints the server's JSON
//! answer, so an agent that cannot use MCP reads it the way it would read the
//! `memory_room_*` tools.

use std::io::Read as _;

use anyhow::Result;
use serde::Serialize;

use crate::cli::{RoomArgs, RoomCommand, RoomListArgs, RoomMemberArgs, RoomPostArgs, RoomReadArgs};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

/// Run `ai-memory room`.
///
/// # Errors
/// An unresolvable scope, an unreadable body, or a refused request.
pub async fn run(config: &Config, args: RoomArgs) -> Result<()> {
    let answer = match args.command {
        RoomCommand::List(args) => list(config, args).await?,
        RoomCommand::Join(args) => membership(config, args, "/admin/rooms/join").await?,
        RoomCommand::Leave(args) => membership(config, args, "/admin/rooms/leave").await?,
        RoomCommand::Post(args) => post(config, args).await?,
        RoomCommand::Read(args) => read(config, args).await?,
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

async fn list(config: &Config, args: RoomListArgs) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let mut query = vec![
        ("workspace", workspace.as_str()),
        ("project", project.as_str()),
    ];
    if let Some(agent) = args.agent.as_deref() {
        query.push(("agent", agent));
    }
    get_json(&endpoint, "/admin/rooms", &query).await
}

#[derive(Serialize)]
struct MembershipRequest<'a> {
    workspace: &'a str,
    project: &'a str,
    room: &'a str,
    agent: &'a str,
}

async fn membership(
    config: &Config,
    args: RoomMemberArgs,
    path: &str,
) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    post_json(
        &endpoint,
        path,
        &MembershipRequest {
            workspace: &workspace,
            project: &project,
            room: &args.room,
            agent: &args.agent,
        },
    )
    .await
}

#[derive(Serialize)]
struct PostRequest<'a> {
    workspace: &'a str,
    project: &'a str,
    room: &'a str,
    agent: &'a str,
    body: &'a str,
}

async fn post(config: &Config, args: RoomPostArgs) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let body = match args.body {
        Some(body) => body,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| anyhow::anyhow!("reading the room post from stdin: {e}"))?;
            buf
        }
    };
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    post_json(
        &endpoint,
        "/admin/rooms/post",
        &PostRequest {
            workspace: &workspace,
            project: &project,
            room: &args.room,
            agent: &args.agent,
            body: &body,
        },
    )
    .await
}

async fn read(config: &Config, args: RoomReadArgs) -> Result<serde_json::Value> {
    let (workspace, project) = scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let after = args.after.map(|after| after.to_string());
    let limit = args.limit.map(|limit| limit.to_string());
    let mut query = vec![
        ("workspace", workspace.as_str()),
        ("project", project.as_str()),
        ("room", args.room.as_str()),
        ("agent", args.agent.as_str()),
    ];
    if let Some(after) = after.as_deref() {
        query.push(("after", after));
    }
    if let Some(limit) = limit.as_deref() {
        query.push(("limit", limit));
    }
    get_json(&endpoint, "/admin/rooms/read", &query).await
}
