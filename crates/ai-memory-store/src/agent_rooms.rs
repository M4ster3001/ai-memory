//! Rooms (V106): named chats among the agents of one project.
//!
//! Writes run in the writer actor, one transaction each. A post auto-joins
//! its poster, keeps the room's history at [`ROOM_HISTORY_CAP`] posts, and
//! turns each `@harness` mention into addressed inbox mail with a fixed body
//! (`ai_memory_core::rooms::mention_body`), in the same transaction. Joins,
//! leaves, posts and mentions are audited without the post text.

use ai_memory_core::rooms::{
    MAX_ROOM_POST_CHARS, MAX_ROOMS_PER_PROJECT, ROOM_HISTORY_CAP, RoomName, mention_body,
    mention_subject, parse_mentions,
};
use ai_memory_core::{
    AgentKind, MessageId, MessageNature, MessagePriority, NewAgentMessage, NotifyOn, ProjectId,
    SessionId, WorkspaceId,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::reader::ReaderPool;
use crate::{MAX_PENDING_INBOX_MESSAGES, StoreError, StoreResult};

/// A room of a project, with its members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSummary {
    /// The room's name.
    pub name: String,
    /// Who created it.
    pub created_by: AgentKind,
    /// When, unix microseconds.
    pub created_at: i64,
    /// Number of the newest post (0 when none).
    pub last_seq: i64,
    /// Members, oldest first.
    pub members: Vec<RoomMember>,
}

/// A member of a room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomMember {
    /// The member harness.
    pub agent: AgentKind,
    /// When it joined, unix microseconds.
    pub joined_at: i64,
    /// The newest post it has read.
    pub read_seq: i64,
}

/// One post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomPost {
    /// Position in the room, from 1.
    pub seq: i64,
    /// The poster.
    pub from_agent: AgentKind,
    /// The text, scrubbed by the caller.
    pub body: String,
    /// The harnesses it mentioned.
    pub mentions: Vec<AgentKind>,
    /// When, unix microseconds.
    pub created_at: i64,
}

/// What a post did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomPostOutcome {
    /// The post's number.
    pub seq: i64,
    /// Mentions that became inbox mail.
    pub mentioned: Vec<(AgentKind, MessageId)>,
    /// Mentions dropped because that inbox was full.
    pub mentions_dropped: Vec<AgentKind>,
}

fn now() -> i64 {
    Timestamp::now().as_microsecond()
}

fn room_id(
    tx: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    name: &RoomName,
) -> StoreResult<Option<Vec<u8>>> {
    Ok(tx
        .query_row(
            "SELECT id FROM agent_rooms WHERE workspace_id = ?1 AND project_id = ?2 AND name = ?3",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                name.as_str()
            ],
            |row| row.get(0),
        )
        .optional()?)
}

fn audit_room(
    tx: &Transaction<'_>,
    op: &str,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    detail: &serde_json::Value,
    at: i64,
) -> StoreResult<()> {
    crate::ops::audit_with_detail(
        tx,
        op,
        Some(workspace_id.as_bytes()),
        Some(project_id.as_bytes()),
        None,
        None,
        at,
        &detail.to_string(),
    )
}

/// Join `agent` to a room, creating the room when it does not exist.
/// Returns `(created, joined)`: whether the room and the membership are new.
pub(crate) fn join_in_tx(
    tx: &Transaction<'_>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    name: &RoomName,
    agent: AgentKind,
    at: i64,
) -> StoreResult<(Vec<u8>, bool, bool)> {
    let (id, created) = match room_id(tx, workspace_id, project_id, name)? {
        Some(id) => (id, false),
        None => {
            let rooms: i64 = tx.query_row(
                "SELECT COUNT(*) FROM agent_rooms WHERE workspace_id = ?1 AND project_id = ?2",
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| row.get(0),
            )?;
            if rooms >= MAX_ROOMS_PER_PROJECT {
                return Err(StoreError::InvalidState(format!(
                    "this project already has {MAX_ROOMS_PER_PROJECT} rooms"
                )));
            }
            let id = uuid::Uuid::now_v7().as_bytes().to_vec();
            tx.execute(
                "INSERT INTO agent_rooms (id, workspace_id, project_id, name, created_by, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    name.as_str(),
                    agent.as_str(),
                    at
                ],
            )?;
            audit_room(
                tx,
                "room_create",
                workspace_id,
                project_id,
                &serde_json::json!({"room": name.as_str(), "agent": agent.as_str()}),
                at,
            )?;
            (id, true)
        }
    };
    // A new member starts at the newest post: joining shows what comes next,
    // and the history stays readable with an explicit `after`.
    let joined = tx.execute(
        "INSERT INTO agent_room_members (room_id, agent_kind, joined_at, read_seq) \
         SELECT ?1, ?2, ?3, last_seq FROM agent_rooms WHERE id = ?1 \
         ON CONFLICT(room_id, agent_kind) DO NOTHING",
        params![id, agent.as_str(), at],
    )? > 0;
    if joined {
        audit_room(
            tx,
            "room_join",
            workspace_id,
            project_id,
            &serde_json::json!({"room": name.as_str(), "agent": agent.as_str()}),
            at,
        )?;
    }
    Ok((id, created, joined))
}

/// Join a room; see [`join_in_tx`].
pub(crate) fn join(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    name: &RoomName,
    agent: AgentKind,
) -> StoreResult<(bool, bool)> {
    let tx = conn.transaction()?;
    let (_, created, joined) = join_in_tx(&tx, workspace_id, project_id, name, agent, now())?;
    tx.commit()?;
    Ok((created, joined))
}

/// Remove `agent` from a room. Returns whether it was a member. The room and
/// its history stay.
pub(crate) fn leave(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    name: &RoomName,
    agent: AgentKind,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let Some(id) = room_id(&tx, workspace_id, project_id, name)? else {
        return Ok(false);
    };
    let left = tx.execute(
        "DELETE FROM agent_room_members WHERE room_id = ?1 AND agent_kind = ?2",
        params![id, agent.as_str()],
    )? > 0;
    if left {
        audit_room(
            &tx,
            "room_leave",
            workspace_id,
            project_id,
            &serde_json::json!({"room": name.as_str(), "agent": agent.as_str()}),
            now(),
        )?;
    }
    tx.commit()?;
    Ok(left)
}

/// What a post needs from its caller.
#[derive(Debug, Clone)]
pub struct NewRoomPost {
    /// The project.
    pub workspace_id: WorkspaceId,
    /// The project.
    pub project_id: ProjectId,
    /// The room; created when missing.
    pub room: RoomName,
    /// The poster.
    pub from_agent: AgentKind,
    /// The poster's recorded hook session, when the server found one.
    pub from_session_id: Option<SessionId>,
    /// The poster's operator storage key, for the mention mail.
    pub from_owner_user: Option<String>,
    /// The text, already scrubbed.
    pub body: String,
}

/// Post to a room: auto-join, append, prune the history, address mentions.
pub(crate) fn post(
    conn: &mut Connection,
    post: &NewRoomPost,
    policy: &ai_memory_core::DeliveryPolicy,
) -> StoreResult<RoomPostOutcome> {
    if post.body.trim().is_empty() || post.body.chars().count() > MAX_ROOM_POST_CHARS {
        return Err(StoreError::InvalidState(format!(
            "a room post is 1 to {MAX_ROOM_POST_CHARS} characters"
        )));
    }
    let at = now();
    let tx = conn.transaction()?;
    let (id, _, _) = join_in_tx(
        &tx,
        post.workspace_id,
        post.project_id,
        &post.room,
        post.from_agent,
        at,
    )?;
    let seq: i64 = tx.query_row(
        "UPDATE agent_rooms SET last_seq = last_seq + 1 WHERE id = ?1 RETURNING last_seq",
        params![id],
        |row| row.get(0),
    )?;
    let mentions = parse_mentions(&post.body, post.from_agent);
    let listed: Vec<&str> = mentions.iter().map(|agent| agent.as_str()).collect();
    tx.execute(
        "INSERT INTO agent_room_posts \
             (room_id, seq, from_agent, from_session_id, body, mentions, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            seq,
            post.from_agent.as_str(),
            post.from_session_id.as_ref().map(|s| s.as_bytes().to_vec()),
            post.body,
            listed.join(","),
            at
        ],
    )?;
    // The poster has read its own post.
    tx.execute(
        "UPDATE agent_room_members SET read_seq = ?3 WHERE room_id = ?1 AND agent_kind = ?2",
        params![id, post.from_agent.as_str(), seq],
    )?;
    tx.execute(
        "DELETE FROM agent_room_posts WHERE room_id = ?1 AND seq <= ?2",
        params![id, seq - ROOM_HISTORY_CAP],
    )?;
    audit_room(
        &tx,
        "room_post",
        post.workspace_id,
        post.project_id,
        &serde_json::json!({
            "room": post.room.as_str(), "agent": post.from_agent.as_str(), "seq": seq,
            "mentions": listed,
        }),
        at,
    )?;
    let mut mentioned = Vec::new();
    let mut mentions_dropped = Vec::new();
    for agent in mentions {
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM agent_messages \
             WHERE to_workspace_id = ?1 AND to_project_id = ?2 AND state = 'pending'",
            params![post.workspace_id.as_bytes(), post.project_id.as_bytes()],
            |row| row.get(0),
        )?;
        if u64::try_from(pending).unwrap_or(u64::MAX) >= MAX_PENDING_INBOX_MESSAGES {
            mentions_dropped.push(agent);
            continue;
        }
        let message = NewAgentMessage {
            from_workspace_id: post.workspace_id,
            from_project_id: post.project_id,
            from_agent: post.from_agent,
            from_session_id: post.from_session_id,
            from_owner_user: post.from_owner_user.clone(),
            to_workspace_id: post.workspace_id,
            to_project_id: post.project_id,
            to_agent: Some(agent),
            subject: Some(mention_subject(&post.room)),
            body: mention_body(&post.room, seq),
            nature: MessageNature::Unknown,
            evidence: None,
            priority: MessagePriority::Normal,
            notify: NotifyOn::NEVER,
            ttl_secs: None,
        };
        let id = crate::ops::insert_message_in_tx(&tx, &message, policy)?;
        mentioned.push((agent, id));
    }
    tx.commit()?;
    Ok(RoomPostOutcome {
        seq,
        mentioned,
        mentions_dropped,
    })
}

/// Advance `agent`'s read position in a room, never backwards. Returns
/// whether it is a member.
pub(crate) fn mark_read(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    name: &RoomName,
    agent: AgentKind,
    seq: i64,
) -> StoreResult<bool> {
    let tx = conn.transaction()?;
    let Some(id) = room_id(&tx, workspace_id, project_id, name)? else {
        return Ok(false);
    };
    let member: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_room_members WHERE room_id = ?1 AND agent_kind = ?2)",
        params![id, agent.as_str()],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE agent_room_members SET read_seq = MAX(read_seq, ?3) \
         WHERE room_id = ?1 AND agent_kind = ?2",
        params![id, agent.as_str(), seq],
    )?;
    tx.commit()?;
    Ok(member)
}

fn members(conn: &Connection, id: &[u8]) -> StoreResult<Vec<RoomMember>> {
    let mut stmt = conn.prepare(
        "SELECT agent_kind, joined_at, read_seq FROM agent_room_members \
         WHERE room_id = ?1 ORDER BY joined_at, agent_kind",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok(RoomMember {
            agent: AgentKind::from_wire(&row.get::<_, String>(0)?),
            joined_at: row.get(1)?,
            read_seq: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn summary(conn: &Connection, row: &rusqlite::Row<'_>) -> StoreResult<RoomSummary> {
    let id: Vec<u8> = row.get(0)?;
    Ok(RoomSummary {
        name: row.get(1)?,
        created_by: AgentKind::from_wire(&row.get::<_, String>(2)?),
        created_at: row.get(3)?,
        last_seq: row.get(4)?,
        members: members(conn, &id)?,
    })
}

/// A room with the names of its project, for the root-only dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomOverview {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// The room.
    pub room: RoomSummary,
}

impl ReaderPool {
    /// Every room on the server, at most `limit`, by project and name. For
    /// the root-only dashboard page.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn rooms_overview(&self, limit: usize) -> StoreResult<Vec<RoomOverview>> {
        let limit = i64::try_from(limit.clamp(1, 1_000)).unwrap_or(1_000);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT r.id, r.name, r.created_by, r.created_at, r.last_seq, w.name, p.name \
                 FROM agent_rooms r \
                 JOIN workspaces w ON w.id = r.workspace_id \
                 JOIN projects p ON p.id = r.project_id \
                 ORDER BY w.name, p.name, r.name LIMIT ?1",
            )?;
            let mut rows = stmt.query(params![limit])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(RoomOverview {
                    workspace: row.get(5)?,
                    project: row.get(6)?,
                    room: summary(conn, row)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The rooms of a project, by name.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn rooms(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<RoomSummary>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, created_by, created_at, last_seq FROM agent_rooms \
                 WHERE workspace_id = ?1 AND project_id = ?2 ORDER BY name",
            )?;
            let mut rows = stmt.query(params![workspace_id.as_bytes(), project_id.as_bytes()])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(summary(conn, row)?);
            }
            Ok(out)
        })
        .await
    }

    /// A room and its posts after `after` (oldest first, at most `limit`),
    /// or `None` when the project has no room of that name.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn room_posts(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        name: RoomName,
        after: i64,
        limit: usize,
    ) -> StoreResult<Option<(RoomSummary, Vec<RoomPost>)>> {
        let limit = i64::try_from(limit.clamp(1, 1_000)).unwrap_or(1_000);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, created_by, created_at, last_seq FROM agent_rooms \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND name = ?3",
            )?;
            let mut rows = stmt.query(params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                name.as_str()
            ])?;
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            let id: Vec<u8> = row.get(0)?;
            let room = summary(conn, row)?;
            let mut posts = conn.prepare(
                "SELECT seq, from_agent, body, mentions, created_at FROM agent_room_posts \
                 WHERE room_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
            )?;
            let posts = posts
                .query_map(params![id, after, limit], |row| {
                    let mentions: String = row.get(3)?;
                    Ok(RoomPost {
                        seq: row.get(0)?,
                        from_agent: AgentKind::from_wire(&row.get::<_, String>(1)?),
                        body: row.get(2)?,
                        mentions: mentions
                            .split(',')
                            .filter(|name| !name.is_empty())
                            .map(AgentKind::from_wire)
                            .collect(),
                        created_at: row.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some((room, posts)))
        })
        .await
    }
}
