//! Store side of the agent bus (`docs/design-agent-bus.md`): the wake handles
//! hook ingest records for each session, and the read the host daemon makes
//! to learn which agents have mail and how to reach them.
//!
//! The daemon never learns a sender, subject or body here. A target is the
//! recipient coordinate, its harness, the newest handle of that harness in the
//! project, and the ids of the mail it could claim.

use ai_memory_core::message_wake::WakeHandleKind;
use ai_memory_core::{AgentKind, MessageId, ProjectId, SessionId, WorkspaceId};
use rusqlite::{OptionalExtension as _, Transaction, params};

use crate::reader::ReaderPool;
use crate::{StoreError, StoreResult};

/// A wake handle refreshes its `last_seen_at` at most once a minute, so a
/// busy session does not rewrite its row on every hook event.
const LAST_SEEN_REFRESH_MICROS: i64 = 60_000_000;

/// Record, in the hook event's own transaction, that `session` can be reached
/// through `kind`. The handle is the session id itself: the router reports a
/// kind only when the native id was a canonical UUID, which the store keeps
/// unchanged.
pub(crate) fn record_wake_handle_in_tx(
    tx: &Transaction<'_>,
    session: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    agent: AgentKind,
    kind: WakeHandleKind,
    now: i64,
) -> StoreResult<()> {
    if WakeHandleKind::for_agent(agent) != Some(kind) {
        return Err(StoreError::InvalidState(
            "a wake handle kind must match the session's harness".into(),
        ));
    }
    tx.execute(
        "INSERT INTO agent_wake_handles \
             (session_id, workspace_id, project_id, agent_kind, handle_kind, handle, \
              first_seen_at, last_seen_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
         ON CONFLICT(session_id) DO UPDATE SET \
             workspace_id = excluded.workspace_id, \
             project_id = excluded.project_id, \
             last_seen_at = excluded.last_seen_at \
         WHERE agent_wake_handles.workspace_id IS NOT excluded.workspace_id \
            OR agent_wake_handles.project_id IS NOT excluded.project_id \
            OR agent_wake_handles.last_seen_at < excluded.last_seen_at - ?8",
        params![
            session.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            agent.as_str(),
            kind.as_str(),
            session.to_string(),
            now,
            LAST_SEEN_REFRESH_MICROS,
        ],
    )?;
    Ok(())
}

/// The newest wake handle of a harness in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeHandle {
    /// How the handle is used.
    pub kind: WakeHandleKind,
    /// The session id, as the harness spelled it.
    pub handle: String,
    /// Whether the session is still open (`sessions.ended_at IS NULL`).
    pub open: bool,
    /// Last hook event seen for the session, unix microseconds (refreshed at
    /// most once a minute).
    pub last_seen_at: i64,
}

/// One agent the daemon may wake: it has claimable mail addressed to it and
/// at least one wake handle in its project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusTarget {
    /// Recipient workspace id.
    pub workspace_id: WorkspaceId,
    /// Recipient workspace name.
    pub workspace: String,
    /// Recipient project id.
    pub project_id: ProjectId,
    /// Recipient project name.
    pub project: String,
    /// The addressed harness.
    pub agent: AgentKind,
    /// Its newest handle in the project.
    pub handle: WakeHandle,
    /// Pending mail addressed to it, oldest first.
    pub pending: Vec<MessageId>,
    /// Whether more mail is pending than `pending` lists.
    pub truncated: bool,
}

impl ReaderPool {
    /// The agents with claimable addressed mail and a wake handle, at most
    /// `max_targets` of them (the ones whose oldest mail waited longest
    /// first), each with at most `max_ids` message ids. Shared project mail
    /// (no `to_agent`) never makes a target: it is not addressed to anyone,
    /// and a room is the way to reach every agent of a project.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn bus_targets(
        &self,
        max_targets: usize,
        max_ids: usize,
    ) -> StoreResult<Vec<BusTarget>> {
        let max_targets = i64::try_from(max_targets.clamp(1, 1_000)).unwrap_or(1_000);
        let max_ids = max_ids.clamp(1, 1_000);
        self.with_conn(move |conn| {
            let mut groups = conn.prepare(
                "SELECT m.to_workspace_id, m.to_project_id, m.to_agent, w.name, p.name, \
                        COUNT(*), MIN(m.created_at) \
                 FROM agent_messages m \
                 JOIN workspaces w ON w.id = m.to_workspace_id \
                 JOIN projects p ON p.id = m.to_project_id \
                 WHERE m.state = 'pending' AND m.to_agent IS NOT NULL \
                   AND EXISTS (SELECT 1 FROM agent_wake_handles h \
                               WHERE h.workspace_id = m.to_workspace_id \
                                 AND h.project_id = m.to_project_id \
                                 AND h.agent_kind = m.to_agent) \
                 GROUP BY m.to_workspace_id, m.to_project_id, m.to_agent \
                 ORDER BY MIN(m.created_at) ASC \
                 LIMIT ?1",
            )?;
            type Group = (Vec<u8>, Vec<u8>, String, String, String, i64);
            let rows: Vec<Group> = groups
                .query_map(params![max_targets], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;
            let mut handles = conn.prepare(
                "SELECT h.handle_kind, h.handle, s.ended_at IS NULL, h.last_seen_at \
                 FROM agent_wake_handles h JOIN sessions s ON s.id = h.session_id \
                 WHERE h.workspace_id = ?1 AND h.project_id = ?2 AND h.agent_kind = ?3 \
                 ORDER BY (s.ended_at IS NULL) DESC, h.last_seen_at DESC LIMIT 1",
            )?;
            let mut ids = conn.prepare(
                "SELECT id FROM agent_messages \
                 WHERE to_workspace_id = ?1 AND to_project_id = ?2 AND state = 'pending' \
                   AND to_agent = ?3 \
                 ORDER BY created_at ASC, id ASC LIMIT ?4",
            )?;
            let mut out = Vec::with_capacity(rows.len());
            for (ws, pj, agent, workspace, project, total) in rows {
                let handle = handles
                    .query_row(params![ws, pj, agent], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    })
                    .optional()?;
                let Some((kind, handle, open, last_seen_at)) = handle else {
                    continue;
                };
                let Some(kind) = WakeHandleKind::parse(&kind) else {
                    continue;
                };
                let pending = ids
                    .query_map(
                        params![ws, pj, agent, i64::try_from(max_ids).unwrap_or(1_000)],
                        |row| row.get::<_, Vec<u8>>(0),
                    )?
                    .map(|id| Ok(MessageId::from_slice(&id?)?))
                    .collect::<StoreResult<Vec<_>>>()?;
                out.push(BusTarget {
                    workspace_id: WorkspaceId::from_slice(&ws)?,
                    workspace,
                    project_id: ProjectId::from_slice(&pj)?,
                    project,
                    agent: AgentKind::from_wire(&agent),
                    handle: WakeHandle {
                        kind,
                        handle,
                        open,
                        last_seen_at,
                    },
                    truncated: usize::try_from(total).unwrap_or(usize::MAX) > pending.len(),
                    pending,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The newest wake handle of each harness in each project, at most
    /// `limit`, newest first, with the project names. For the root-only
    /// dashboard page; the handle ids stay on the server.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn wake_handles_overview(
        &self,
        limit: usize,
    ) -> StoreResult<Vec<(String, String, AgentKind, WakeHandle)>> {
        let limit = i64::try_from(limit.clamp(1, 1_000)).unwrap_or(1_000);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT w.name, p.name, h.agent_kind, h.handle_kind, h.handle, \
                        s.ended_at IS NULL, MAX(h.last_seen_at) \
                 FROM agent_wake_handles h \
                 JOIN sessions s ON s.id = h.session_id \
                 JOIN workspaces w ON w.id = h.workspace_id \
                 JOIN projects p ON p.id = h.project_id \
                 GROUP BY h.workspace_id, h.project_id, h.agent_kind \
                 ORDER BY MAX(h.last_seen_at) DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (workspace, project, agent, kind, handle, open, last_seen_at) = row?;
                if let Some(kind) = WakeHandleKind::parse(&kind) {
                    out.push((
                        workspace,
                        project,
                        AgentKind::from_wire(&agent),
                        WakeHandle {
                            kind,
                            handle,
                            open,
                            last_seen_at,
                        },
                    ));
                }
            }
            Ok(out)
        })
        .await
    }

    /// Every wake handle of a project, newest first, for the dashboard and
    /// tests.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn wake_handles(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<Vec<(AgentKind, WakeHandle)>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT h.agent_kind, h.handle_kind, h.handle, s.ended_at IS NULL, h.last_seen_at \
                 FROM agent_wake_handles h JOIN sessions s ON s.id = h.session_id \
                 WHERE h.workspace_id = ?1 AND h.project_id = ?2 \
                 ORDER BY h.last_seen_at DESC LIMIT 200",
            )?;
            let rows = stmt.query_map(
                params![workspace_id.as_bytes(), project_id.as_bytes()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )?;
            let mut out = Vec::new();
            for row in rows {
                let (agent, kind, handle, open, last_seen_at) = row?;
                if let Some(kind) = WakeHandleKind::parse(&kind) {
                    out.push((
                        AgentKind::from_wire(&agent),
                        WakeHandle {
                            kind,
                            handle,
                            open,
                            last_seen_at,
                        },
                    ));
                }
            }
            Ok(out)
        })
        .await
    }
}
