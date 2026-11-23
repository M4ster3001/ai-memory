//! Links between two projects (V109, `docs/agent-bus/CROSS_PROJECT.md`).
//!
//! A link is requested, confirmed by the destination project, and then lets
//! mail between exactly those two projects wake like mail inside one
//! project. Every rule here runs in the writer, in one transaction, without
//! a model and without reading a message body:
//!
//! - a request never wakes anyone;
//! - only the destination confirms: the root user on the destination's page,
//!   or an agent of the destination whose session was shown the request and
//!   then saw a user prompt (a wake text is not one);
//! - the requester never confirms its own request;
//! - a link ends at its clock, when either side closes it, or when a project
//!   is purged;
//! - every step is an event without message text, and the request, the
//!   confirmation and the end are also written to `audit_log`.
//!
//! The wake gate (`wake_candidates`) reads `agent_links` itself and compares
//! `expires_at` with the clock, so a link whose sweep has not run yet still
//! never wakes anyone.

use ai_memory_core::agent_links::{
    CONFIRM_PROMPT_MAX_AGE_SECS, CloseReason, ConfirmRefusal, ConfirmVia, LinkState,
    MAX_AGENT_REQUESTS_PER_HOUR, MAX_LIVE_LINKS_PER_PROJECT, REQUEST_TTL_SECS, is_past,
};
use ai_memory_core::{AgentKind, AgentLinkId, ProjectId, SessionId, WorkspaceId};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::reader::ReaderPool;
use crate::{StoreError, StoreResult};

const MICROS: i64 = 1_000_000;

fn now() -> i64 {
    Timestamp::now().as_microsecond()
}

/// A project, by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectKey {
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Project id.
    pub project_id: ProjectId,
}

/// Who files a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRequester {
    /// The root user, on the web page.
    Human,
    /// An agent, with its recorded hook session when the server found one.
    Agent {
        /// Its harness.
        agent: AgentKind,
        /// The recorded session, when the caller has one.
        session: Option<SessionId>,
    },
}

/// A request to link two projects.
#[derive(Debug, Clone)]
pub struct NewLinkRequest {
    /// The side that asks.
    pub from: ProjectKey,
    /// The destination, which confirms.
    pub to: ProjectKey,
    /// Why, already validated by `ai_memory_core::agent_links::link_reason`.
    pub reason: String,
    /// Validity once confirmed, validated by `link_validity`.
    pub valid_secs: u64,
    /// Who asks.
    pub by: LinkRequester,
    /// The root user also confirms for the destination in the same act.
    /// Ignored for an agent.
    pub confirm_now: bool,
}

/// Why a request was not filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRefusal {
    /// Both sides are the same project.
    SameProject,
    /// A side already holds the most live links a project may.
    TooManyLive,
    /// An agent of the asking project filed too many requests this hour.
    TooManyRecent,
}

impl RequestRefusal {
    /// A plain sentence for the caller.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::SameProject => "a link joins two different projects",
            Self::TooManyLive => {
                "one of the projects already has the most live links it may; close one first"
            }
            Self::TooManyRecent => "too many requests from this project this hour; wait a while",
        }
    }
}

/// What a request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRequestOutcome {
    /// A new link was filed, in this state.
    Created(AgentLinkId, LinkState),
    /// The pair already had a live link, in this state; nothing changed.
    Existing(AgentLinkId, LinkState),
    /// Nothing was filed.
    Refused(RequestRefusal),
}

/// What a confirmation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkConfirmOutcome {
    /// The link is active until `expires_at` (unix microseconds).
    Confirmed {
        /// When it ends.
        expires_at: i64,
    },
    /// The rule that refused.
    Refused(ConfirmRefusal),
}

/// Who closes a link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCloser {
    /// The root user.
    Human,
    /// An agent, which must belong to one of the two projects.
    Agent {
        /// The agent's project.
        project: ProjectKey,
        /// Its harness.
        agent: AgentKind,
    },
}

/// What a close did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCloseOutcome {
    /// The link ended; it was in this state.
    Closed(LinkState),
    /// No such link.
    NotFound,
    /// The link was already over.
    NotLive,
    /// An agent of neither project tried.
    NotAllowed,
}

fn audit(
    tx: &Transaction<'_>,
    op: &str,
    project: Option<ProjectKey>,
    detail: &serde_json::Value,
    at: i64,
) -> StoreResult<()> {
    let (workspace, project) = match project {
        Some(p) => (
            Some(*p.workspace_id.as_bytes()),
            Some(*p.project_id.as_bytes()),
        ),
        None => (None, None),
    };
    crate::ops::audit_with_detail(
        tx,
        op,
        workspace.as_ref(),
        project.as_ref(),
        None,
        None,
        at,
        &detail.to_string(),
    )
}

fn event(
    tx: &Transaction<'_>,
    link: &AgentLinkId,
    at: i64,
    event: &str,
    actor: &str,
    detail: &str,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO agent_link_events (link_id, at, event, actor, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![link.as_bytes(), at, event, actor, detail],
    )?;
    Ok(())
}

/// A link row as the rules read it.
struct Row {
    id: AgentLinkId,
    from: ProjectKey,
    to: ProjectKey,
    state: LinkState,
    valid_secs: i64,
    requested_session: Option<SessionId>,
    presented_at: Option<i64>,
    presented_session: Option<SessionId>,
}

fn key(ws: &[u8], pj: &[u8]) -> StoreResult<ProjectKey> {
    Ok(ProjectKey {
        workspace_id: WorkspaceId::from_slice(ws)?,
        project_id: ProjectId::from_slice(pj)?,
    })
}

fn session(raw: Option<Vec<u8>>) -> StoreResult<Option<SessionId>> {
    raw.map(|bytes| SessionId::from_slice(&bytes))
        .transpose()
        .map_err(StoreError::from)
}

fn load(tx: &Transaction<'_>, id: AgentLinkId) -> StoreResult<Option<Row>> {
    let raw = tx
        .query_row(
            "SELECT from_workspace_id, from_project_id, to_workspace_id, to_project_id, state, \
                    valid_secs, requested_session, presented_at, presented_session \
             FROM agent_links WHERE id = ?1",
            params![id.as_bytes()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<Vec<u8>>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<Vec<u8>>>(8)?,
                ))
            },
        )
        .optional()?;
    let Some((fw, fp, tw, tp, state, valid_secs, requested, presented_at, presented)) = raw else {
        return Ok(None);
    };
    let Some(state) = LinkState::parse(&state) else {
        return Ok(None);
    };
    Ok(Some(Row {
        id,
        from: key(&fw, &fp)?,
        to: key(&tw, &tp)?,
        state,
        valid_secs,
        requested_session: session(requested)?,
        presented_at,
        presented_session: session(presented)?,
    }))
}

/// End every live link whose clock ran out at `at`. Returns how many.
pub(crate) fn expire_due_in_tx(tx: &Transaction<'_>, at: i64) -> StoreResult<usize> {
    type Due = (Vec<u8>, Vec<u8>, Vec<u8>, i64);
    let due: Vec<Due> = {
        let mut stmt = tx.prepare(
            "SELECT id, from_project_id, from_workspace_id, expires_at FROM agent_links \
             WHERE state IN ('requested','active') AND expires_at <= ?1",
        )?;
        stmt.query_map(params![at], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?
    };
    for (id, project, workspace, expires_at) in &due {
        tx.execute(
            "UPDATE agent_links SET state = 'expired', closed_at = ?2, closed_by = 'clock', \
                 close_reason = 'expired' WHERE id = ?1",
            params![id, expires_at],
        )?;
        let link = AgentLinkId::from_slice(id)?;
        event(tx, &link, at, "expired", "clock", "")?;
        audit(
            tx,
            "agent_link_expired",
            Some(key(workspace, project)?),
            &serde_json::json!({"link": link.to_string()}),
            at,
        )?;
    }
    Ok(due.len())
}

/// End every live link that has `project` as a side, because the project is
/// being removed. Called inside the purge transaction, before the rows go.
pub(crate) fn close_for_project_in_tx(
    tx: &Transaction<'_>,
    project: ProjectKey,
    at: i64,
) -> StoreResult<usize> {
    close_where(
        tx,
        "(from_project_id = ?1 OR to_project_id = ?1)",
        project.project_id.as_bytes(),
        project,
        at,
    )
}

/// End every live link that has a project of `workspace` as a side, because
/// the workspace is being removed.
pub(crate) fn close_for_workspace_in_tx(
    tx: &Transaction<'_>,
    workspace: WorkspaceId,
    at: i64,
) -> StoreResult<usize> {
    let scope = ProjectKey {
        workspace_id: workspace,
        project_id: ProjectId::from_slice(&[0; 16])?,
    };
    close_where(
        tx,
        "(from_workspace_id = ?1 OR to_workspace_id = ?1)",
        workspace.as_bytes(),
        scope,
        at,
    )
}

fn close_where(
    tx: &Transaction<'_>,
    predicate: &str,
    id: &[u8; 16],
    scope: ProjectKey,
    at: i64,
) -> StoreResult<usize> {
    let live: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id FROM agent_links WHERE state IN ('requested','active') AND {predicate}"
        ))?;
        stmt.query_map(params![id], |row| row.get(0))?
            .collect::<Result<_, _>>()?
    };
    for link_id in &live {
        tx.execute(
            "UPDATE agent_links SET state = 'closed', closed_at = ?2, closed_by = 'server', \
                 close_reason = 'project_purged' WHERE id = ?1",
            params![link_id, at],
        )?;
        let link = AgentLinkId::from_slice(link_id)?;
        event(tx, &link, at, "closed", "server", "project_purged")?;
        audit(
            tx,
            "agent_link_closed",
            Some(scope),
            &serde_json::json!({"link": link.to_string(), "reason": "project_purged"}),
            at,
        )?;
    }
    Ok(live.len())
}

fn live_count(tx: &Transaction<'_>, project: ProjectKey) -> StoreResult<i64> {
    Ok(tx.query_row(
        "SELECT COUNT(*) FROM agent_links WHERE state IN ('requested','active') \
           AND (from_project_id = ?1 OR to_project_id = ?1)",
        params![project.project_id.as_bytes()],
        |row| row.get(0),
    )?)
}

/// File a request (see [`NewLinkRequest`]).
pub(crate) fn request(
    conn: &mut Connection,
    request: &NewLinkRequest,
    at: i64,
) -> StoreResult<LinkRequestOutcome> {
    if request.from.project_id == request.to.project_id {
        return Ok(LinkRequestOutcome::Refused(RequestRefusal::SameProject));
    }
    let tx = conn.transaction()?;
    expire_due_in_tx(&tx, at)?;
    let existing = tx
        .query_row(
            "SELECT id, state FROM agent_links \
             WHERE state IN ('requested','active') \
               AND MIN(from_project_id, to_project_id) = MIN(?1, ?2) \
               AND MAX(from_project_id, to_project_id) = MAX(?1, ?2)",
            params![
                request.from.project_id.as_bytes(),
                request.to.project_id.as_bytes()
            ],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((id, state)) = existing
        && let Some(state) = LinkState::parse(&state)
    {
        tx.commit()?;
        return Ok(LinkRequestOutcome::Existing(
            AgentLinkId::from_slice(&id)?,
            state,
        ));
    }
    if live_count(&tx, request.from)? >= MAX_LIVE_LINKS_PER_PROJECT
        || live_count(&tx, request.to)? >= MAX_LIVE_LINKS_PER_PROJECT
    {
        tx.commit()?;
        return Ok(LinkRequestOutcome::Refused(RequestRefusal::TooManyLive));
    }
    let (by, agent, requested_session) = match request.by {
        LinkRequester::Human => ("human", None, None),
        LinkRequester::Agent { agent, session } => {
            // Only a session the server recorded in the asking project is
            // provenance; any other id is dropped, never trusted.
            let recorded = match session {
                Some(session) => tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1 AND project_id = ?2)",
                    params![session.as_bytes(), request.from.project_id.as_bytes()],
                    |row| row.get::<_, bool>(0),
                )?,
                None => false,
            };
            ("agent", Some(agent), session.filter(|_| recorded))
        }
    };
    if by == "agent" {
        let recent: i64 = tx.query_row(
            "SELECT COUNT(*) FROM agent_links WHERE requested_by = 'agent' \
               AND from_project_id = ?1 AND requested_at > ?2",
            params![request.from.project_id.as_bytes(), at - 3_600 * MICROS],
            |row| row.get(0),
        )?;
        if recent >= MAX_AGENT_REQUESTS_PER_HOUR {
            tx.commit()?;
            return Ok(LinkRequestOutcome::Refused(RequestRefusal::TooManyRecent));
        }
    }
    let id = AgentLinkId::new();
    let valid_micros = i64::try_from(request.valid_secs).unwrap_or(0) * MICROS;
    let both = by == "human" && request.confirm_now;
    let (state, confirmed_at, via, expires_at) = if both {
        (
            LinkState::Active,
            Some(at),
            Some(ConfirmVia::WebBoth.as_str()),
            at + valid_micros,
        )
    } else {
        (
            LinkState::Requested,
            None,
            None,
            at + i64::try_from(REQUEST_TTL_SECS).unwrap_or(0) * MICROS,
        )
    };
    tx.execute(
        "INSERT INTO agent_links \
             (id, from_workspace_id, from_project_id, to_workspace_id, to_project_id, state, \
              reason, valid_secs, requested_at, requested_by, requested_agent, requested_session, \
              confirmed_at, confirmed_via, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            id.as_bytes(),
            request.from.workspace_id.as_bytes(),
            request.from.project_id.as_bytes(),
            request.to.workspace_id.as_bytes(),
            request.to.project_id.as_bytes(),
            state.as_str(),
            request.reason,
            i64::try_from(request.valid_secs).unwrap_or(0),
            at,
            by,
            agent.map(AgentKind::as_str),
            requested_session.as_ref().map(|s| s.as_bytes().to_vec()),
            confirmed_at,
            via,
            expires_at,
        ],
    )?;
    let actor = agent.map_or_else(|| "human".to_owned(), |a| format!("agent:{}", a.as_str()));
    event(
        &tx,
        &id,
        at,
        "requested",
        &actor,
        &format!("valid_secs={}", request.valid_secs),
    )?;
    audit(
        &tx,
        "agent_link_requested",
        Some(request.from),
        &serde_json::json!({
            "link": id.to_string(),
            "to_project_id": request.to.project_id.to_string(),
            "by": by,
            "valid_secs": request.valid_secs,
        }),
        at,
    )?;
    if both {
        event(&tx, &id, at, "confirmed", "human", "via=web_both")?;
        audit(
            &tx,
            "agent_link_confirmed",
            Some(request.to),
            &serde_json::json!({"link": id.to_string(), "via": "web_both"}),
            at,
        )?;
    }
    tx.commit()?;
    Ok(LinkRequestOutcome::Created(id, state))
}

/// Show the requests waiting for `project` to the agent session `session`:
/// the evidence an agent's confirmation needs starts here. Returns the ids
/// of the requests the session had not been shown yet.
pub(crate) fn present(
    conn: &mut Connection,
    project: ProjectKey,
    agent: AgentKind,
    session: SessionId,
    at: i64,
) -> StoreResult<Vec<AgentLinkId>> {
    let tx = conn.transaction()?;
    expire_due_in_tx(&tx, at)?;
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1 AND project_id = ?2)",
        params![session.as_bytes(), project.project_id.as_bytes()],
        |row| row.get(0),
    )?;
    if !known {
        tx.commit()?;
        return Ok(Vec::new());
    }
    let fresh: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM agent_links WHERE state = 'requested' AND to_project_id = ?1 \
               AND (presented_session IS NULL OR presented_session <> ?2)",
        )?;
        stmt.query_map(
            params![project.project_id.as_bytes(), session.as_bytes()],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?
    };
    let mut shown = Vec::with_capacity(fresh.len());
    for id in fresh {
        tx.execute(
            "UPDATE agent_links SET presented_at = ?2, presented_session = ?3 WHERE id = ?1",
            params![id, at, session.as_bytes()],
        )?;
        let link = AgentLinkId::from_slice(&id)?;
        event(
            &tx,
            &link,
            at,
            "presented",
            &format!("agent:{}", agent.as_str()),
            "",
        )?;
        shown.push(link);
    }
    tx.commit()?;
    Ok(shown)
}

fn activate(
    tx: &Transaction<'_>,
    row: &Row,
    at: i64,
    via: ConfirmVia,
    agent: Option<(AgentKind, SessionId)>,
) -> StoreResult<i64> {
    let expires_at = at + row.valid_secs * MICROS;
    tx.execute(
        "UPDATE agent_links SET state = 'active', confirmed_at = ?2, confirmed_via = ?3, \
             confirmed_agent = ?4, confirmed_session = ?5, expires_at = ?6 WHERE id = ?1",
        params![
            row.id.as_bytes(),
            at,
            via.as_str(),
            agent.map(|(a, _)| a.as_str()),
            agent.map(|(_, s)| s.as_bytes().to_vec()),
            expires_at
        ],
    )?;
    let actor = agent.map_or_else(
        || "human".to_owned(),
        |(agent, _)| format!("agent:{}", agent.as_str()),
    );
    event(
        tx,
        &row.id,
        at,
        "confirmed",
        &actor,
        &format!("via={}", via.as_str()),
    )?;
    audit(
        tx,
        "agent_link_confirmed",
        Some(row.to),
        &serde_json::json!({
            "link": row.id.to_string(),
            "via": via.as_str(),
            "from_project_id": row.from.project_id.to_string(),
        }),
        at,
    )?;
    Ok(expires_at)
}

fn refuse(
    tx: Transaction<'_>,
    row: &Row,
    refusal: ConfirmRefusal,
    actor: &str,
    at: i64,
) -> StoreResult<LinkConfirmOutcome> {
    event(&tx, &row.id, at, "confirm_refused", actor, refusal.code())?;
    tx.commit()?;
    Ok(LinkConfirmOutcome::Refused(refusal))
}

/// Confirm from the destination project's web page. `viewing` is the project
/// whose page the root user pressed the button on; it must be the
/// destination.
pub(crate) fn confirm_web(
    conn: &mut Connection,
    id: AgentLinkId,
    viewing: ProjectKey,
    at: i64,
) -> StoreResult<LinkConfirmOutcome> {
    let tx = conn.transaction()?;
    expire_due_in_tx(&tx, at)?;
    let Some(row) = load(&tx, id)? else {
        tx.commit()?;
        return Ok(LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested));
    };
    if row.state != LinkState::Requested {
        tx.commit()?;
        return Ok(LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested));
    }
    if row.to != viewing {
        return refuse(tx, &row, ConfirmRefusal::NotDestination, "human", at);
    }
    let expires_at = activate(&tx, &row, at, ConfirmVia::Web, None)?;
    tx.commit()?;
    Ok(LinkConfirmOutcome::Confirmed { expires_at })
}

/// What an agent's confirmation needs from its caller.
#[derive(Debug, Clone, Copy)]
pub struct AgentConfirm {
    /// The link.
    pub link: AgentLinkId,
    /// The agent's project.
    pub project: ProjectKey,
    /// Its harness.
    pub agent: AgentKind,
    /// Its recorded hook session.
    pub session: Option<SessionId>,
}

/// Whether `session` has a user prompt after `after` (and not older than
/// the confirmation window). A prompt that is only a wake text is not one.
fn has_user_answer(
    tx: &Transaction<'_>,
    session: SessionId,
    project: ProjectKey,
    after: i64,
    at: i64,
) -> StoreResult<bool> {
    let floor = after.max(at - CONFIRM_PROMPT_MAX_AGE_SECS * MICROS);
    let mut stmt = tx.prepare(
        "SELECT body FROM observations \
         WHERE session_id = ?1 AND project_id = ?2 AND kind = 'user-prompt' AND created_at > ?3 \
         ORDER BY created_at DESC LIMIT 20",
    )?;
    let bodies = stmt.query_map(
        params![session.as_bytes(), project.project_id.as_bytes(), floor],
        |row| row.get::<_, String>(0),
    )?;
    for body in bodies {
        let body = body?;
        if !body.trim().is_empty() && !ai_memory_core::message_wake::is_wake_text(&body) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Confirm as an agent of the destination project.
pub(crate) fn confirm_agent(
    conn: &mut Connection,
    confirm: &AgentConfirm,
    at: i64,
) -> StoreResult<LinkConfirmOutcome> {
    let actor = format!("agent:{}", confirm.agent.as_str());
    let tx = conn.transaction()?;
    expire_due_in_tx(&tx, at)?;
    let Some(row) = load(&tx, confirm.link)? else {
        tx.commit()?;
        return Ok(LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested));
    };
    if row.state != LinkState::Requested {
        tx.commit()?;
        return Ok(LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested));
    }
    if row.to != confirm.project {
        return refuse(tx, &row, ConfirmRefusal::NotDestination, &actor, at);
    }
    let Some(session) = confirm.session else {
        return refuse(tx, &row, ConfirmRefusal::NoSession, &actor, at);
    };
    let recorded: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1 AND project_id = ?2)",
        params![session.as_bytes(), confirm.project.project_id.as_bytes()],
        |r| r.get(0),
    )?;
    if !recorded {
        return refuse(tx, &row, ConfirmRefusal::NoSession, &actor, at);
    }
    if row.requested_session == Some(session) {
        return refuse(tx, &row, ConfirmRefusal::OwnRequest, &actor, at);
    }
    let shown_at = match (row.presented_at, row.presented_session) {
        (Some(shown_at), Some(shown_to)) if shown_to == session => shown_at,
        _ => return refuse(tx, &row, ConfirmRefusal::NotPresented, &actor, at),
    };
    if !has_user_answer(&tx, session, confirm.project, shown_at, at)? {
        return refuse(tx, &row, ConfirmRefusal::NoUserAnswer, &actor, at);
    }
    let expires_at = activate(
        &tx,
        &row,
        at,
        ConfirmVia::Agent,
        Some((confirm.agent, session)),
    )?;
    tx.commit()?;
    Ok(LinkConfirmOutcome::Confirmed { expires_at })
}

/// End a link.
pub(crate) fn close(
    conn: &mut Connection,
    id: AgentLinkId,
    closer: LinkCloser,
    at: i64,
) -> StoreResult<LinkCloseOutcome> {
    let tx = conn.transaction()?;
    expire_due_in_tx(&tx, at)?;
    let Some(row) = load(&tx, id)? else {
        tx.commit()?;
        return Ok(LinkCloseOutcome::NotFound);
    };
    if !row.state.is_live() {
        tx.commit()?;
        return Ok(LinkCloseOutcome::NotLive);
    }
    if let LinkCloser::Agent { project, .. } = closer
        && project != row.from
        && project != row.to
    {
        tx.commit()?;
        return Ok(LinkCloseOutcome::NotAllowed);
    }
    let actor = match closer {
        LinkCloser::Human => "human".to_owned(),
        LinkCloser::Agent { agent, .. } => format!("agent:{}", agent.as_str()),
    };
    tx.execute(
        "UPDATE agent_links SET state = 'closed', closed_at = ?2, closed_by = ?3, \
             close_reason = ?4 WHERE id = ?1",
        params![id.as_bytes(), at, actor, CloseReason::Closed.as_str()],
    )?;
    event(&tx, &id, at, "closed", &actor, row.state.as_str())?;
    audit(
        &tx,
        "agent_link_closed",
        Some(row.from),
        &serde_json::json!({
            "link": id.to_string(),
            "was": row.state.as_str(),
            "by": actor,
            "to_project_id": row.to.project_id.to_string(),
        }),
        at,
    )?;
    tx.commit()?;
    Ok(LinkCloseOutcome::Closed(row.state))
}

/// Note, in the link's trail, that mail from a linked project reached a wake
/// outcome. Called from the wake report; no text, only the outcome word.
pub(crate) fn note_wake_in_tx(
    tx: &Transaction<'_>,
    message: &[u8],
    outcome: &str,
    at: i64,
) -> StoreResult<()> {
    let link: Option<Vec<u8>> = tx
        .query_row(
            "SELECT l.id FROM agent_messages m JOIN agent_links l \
               ON l.state = 'active' AND l.expires_at > ?2 \
              AND ((l.from_project_id = m.from_project_id AND l.to_project_id = m.to_project_id) \
                OR (l.from_project_id = m.to_project_id AND l.to_project_id = m.from_project_id)) \
             WHERE m.id = ?1 AND m.from_project_id <> m.to_project_id",
            params![message, at],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(link) = link {
        event(
            tx,
            &AgentLinkId::from_slice(&link)?,
            at,
            "wake",
            "server",
            outcome,
        )?;
    }
    Ok(())
}

/// A project of a link, with its names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkProject {
    /// Ids.
    pub key: ProjectKey,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
}

/// A link as the pages and tools show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkView {
    /// The link.
    pub id: AgentLinkId,
    /// The side that asked.
    pub from: LinkProject,
    /// The destination.
    pub to: LinkProject,
    /// The state at read time: a live link past its clock reads `expired`
    /// even before the sweep ends it.
    pub state: LinkState,
    /// Why it was asked (untrusted text when an agent wrote it).
    pub reason: String,
    /// Validity once confirmed, seconds.
    pub valid_secs: i64,
    /// When it was asked, unix microseconds.
    pub requested_at: i64,
    /// Whether the root user asked (else an agent).
    pub requested_by_human: bool,
    /// The harness that asked, when an agent did.
    pub requested_agent: Option<AgentKind>,
    /// Whether the destination's agent was shown the request.
    pub presented: bool,
    /// When the destination confirmed.
    pub confirmed_at: Option<i64>,
    /// How: `web`, `web_both` or `agent`.
    pub confirmed_via: Option<String>,
    /// The end of the request or of the link.
    pub expires_at: i64,
    /// When it ended, when it has.
    pub closed_at: Option<i64>,
    /// Why it ended: `closed`, `expired` or `project_purged`.
    pub close_reason: Option<String>,
    /// Who closed it.
    pub closed_by: Option<String>,
}

impl LinkView {
    /// The other side, seen from `project`.
    #[must_use]
    pub fn other_side(&self, project: ProjectId) -> &LinkProject {
        if self.from.key.project_id == project {
            &self.to
        } else {
            &self.from
        }
    }
}

const VIEW_COLUMNS: &str = "l.id, l.from_workspace_id, l.from_project_id, fw.name, fp.name, \
     l.to_workspace_id, l.to_project_id, tw.name, tp.name, l.state, l.reason, l.valid_secs, \
     l.requested_at, l.requested_by, l.requested_agent, l.presented_at IS NOT NULL, \
     l.confirmed_at, l.confirmed_via, l.expires_at, l.closed_at, l.close_reason, l.closed_by";

const VIEW_JOINS: &str = "FROM agent_links l \
     JOIN workspaces fw ON fw.id = l.from_workspace_id \
     JOIN projects fp ON fp.id = l.from_project_id \
     JOIN workspaces tw ON tw.id = l.to_workspace_id \
     JOIN projects tp ON tp.id = l.to_project_id";

fn view(row: &rusqlite::Row<'_>, at: i64) -> StoreResult<Option<LinkView>> {
    let state = LinkState::parse(&row.get::<_, String>(9)?);
    let Some(mut state) = state else {
        return Ok(None);
    };
    let expires_at: i64 = row.get(18)?;
    if state.is_live() && is_past(expires_at, at) {
        state = LinkState::Expired;
    }
    Ok(Some(LinkView {
        id: AgentLinkId::from_slice(&row.get::<_, Vec<u8>>(0)?)?,
        from: LinkProject {
            key: key(&row.get::<_, Vec<u8>>(1)?, &row.get::<_, Vec<u8>>(2)?)?,
            workspace: row.get(3)?,
            project: row.get(4)?,
        },
        to: LinkProject {
            key: key(&row.get::<_, Vec<u8>>(5)?, &row.get::<_, Vec<u8>>(6)?)?,
            workspace: row.get(7)?,
            project: row.get(8)?,
        },
        state,
        reason: row.get(10)?,
        valid_secs: row.get(11)?,
        requested_at: row.get(12)?,
        requested_by_human: row.get::<_, String>(13)? == "human",
        requested_agent: row
            .get::<_, Option<String>>(14)?
            .map(|agent| AgentKind::from_wire(&agent)),
        presented: row.get(15)?,
        confirmed_at: row.get(16)?,
        confirmed_via: row.get(17)?,
        expires_at,
        closed_at: row.get(19)?,
        close_reason: row.get(20)?,
        closed_by: row.get(21)?,
    }))
}

/// One interval during which two projects were linked and mail between them
/// may be shown with its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkWindow {
    /// The other project of the pair.
    pub other: ProjectId,
    /// Start: the confirmation, unix microseconds.
    pub from: i64,
    /// End: the close, else the clock end.
    pub until: i64,
}

pub(crate) fn link_windows(conn: &Connection, project: ProjectId) -> StoreResult<Vec<LinkWindow>> {
    let mut stmt = conn.prepare(
        "SELECT CASE WHEN from_project_id = ?1 THEN to_project_id ELSE from_project_id END, \
                confirmed_at, COALESCE(closed_at, expires_at) \
         FROM agent_links \
         WHERE confirmed_at IS NOT NULL AND (from_project_id = ?1 OR to_project_id = ?1)",
    )?;
    let rows = stmt.query_map(params![project.as_bytes()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (other, from, until) = row?;
        out.push(LinkWindow {
            other: ProjectId::from_slice(&other)?,
            from,
            until,
        });
    }
    Ok(out)
}

impl ReaderPool {
    /// Links, newest request first. `project` limits them to those that have
    /// it as a side; `include_ended` adds closed and expired ones.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn agent_links(
        &self,
        project: Option<ProjectKey>,
        include_ended: bool,
        limit: usize,
    ) -> StoreResult<Vec<LinkView>> {
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(500);
        self.agent_links_at(project, include_ended, limit, now())
            .await
    }

    /// [`Self::agent_links`] as the links read at `at` (unix microseconds).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn agent_links_at(
        &self,
        project: Option<ProjectKey>,
        include_ended: bool,
        limit: i64,
        at: i64,
    ) -> StoreResult<Vec<LinkView>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {VIEW_COLUMNS} {VIEW_JOINS} \
                 WHERE (?1 IS NULL OR l.from_project_id = ?1 OR l.to_project_id = ?1) \
                   AND (?2 OR (l.state IN ('requested','active') AND l.expires_at > ?3)) \
                 ORDER BY l.requested_at DESC LIMIT ?4"
            ))?;
            let mut rows = stmt.query(params![
                project.map(|p| p.project_id.as_bytes().to_vec()),
                include_ended,
                at,
                limit
            ])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                if let Some(link) = view(row, at)? {
                    out.push(link);
                }
            }
            Ok(out)
        })
        .await
    }

    /// One link by id, as read now.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn agent_link(&self, id: AgentLinkId) -> StoreResult<Option<LinkView>> {
        self.with_conn(move |conn| {
            let at = now();
            let mut stmt = conn.prepare(&format!(
                "SELECT {VIEW_COLUMNS} {VIEW_JOINS} WHERE l.id = ?1"
            ))?;
            let mut rows = stmt.query(params![id.as_bytes()])?;
            match rows.next()? {
                Some(row) => view(row, at),
                None => Ok(None),
            }
        })
        .await
    }

    /// Whether `a` and `b` are joined by an active link at `at`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn projects_linked_at(
        &self,
        a: ProjectId,
        b: ProjectId,
        at: i64,
    ) -> StoreResult<bool> {
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_links WHERE state = 'active' \
                   AND expires_at > ?3 \
                   AND ((from_project_id = ?1 AND to_project_id = ?2) \
                     OR (from_project_id = ?2 AND to_project_id = ?1)))",
                params![a.as_bytes(), b.as_bytes(), at],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// The trail of one link, oldest first: `(at, event, actor, detail)`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn agent_link_events(
        &self,
        id: AgentLinkId,
        limit: usize,
    ) -> StoreResult<Vec<(i64, String, String, String)>> {
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(500);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT at, event, actor, detail FROM agent_link_events \
                 WHERE link_id = ?1 ORDER BY id LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![id.as_bytes(), limit], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .await
    }
}
