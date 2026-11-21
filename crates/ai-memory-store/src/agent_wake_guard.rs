//! The loop guard of the agent bus (`docs/design-agent-bus.md`, slice F3).
//!
//! Wakes have no hourly budget. What stops two agents from waking each other
//! forever is a circuit breaker per pair of agents (a harness in a project),
//! tripped by signals only a loop produces and computed here, in the writer,
//! without a model:
//!
//! - ping-pong: the pair's addressed mail changed direction
//!   [`PING_PONG_TURNS`] times since a human last prompted either side;
//! - repeated content: the same normalized body [`REPEATED_CONTENT`] times in
//!   the pair within [`REPEAT_WINDOW_MICROS`];
//! - thread depth: the message is the end of a chain of more than
//!   [`MAX_THREAD_DEPTH`] agent turns, each woken by mail, with no human
//!   prompt to the sender in between. This catches a loop among three or
//!   more agents that no single pair shows.
//!
//! A tripped pair keeps receiving mail; the host supervisor and the Claude
//! Code wake hook stop waking either side for mail between the two and
//! report `wake_failed/loop_suspected`. A human prompt in a session of either
//! side, or the dashboard's resume button, closes the breaker. Every pause
//! and resume is audited without message text.

use ai_memory_core::{AgentKind, ProjectId, WorkspaceId};
use rusqlite::{OptionalExtension as _, Transaction, params};

use crate::StoreResult;
use crate::reader::ReaderPool;

/// Direction changes of a pair's mail, since its anchor, that trip the
/// breaker.
pub const PING_PONG_TURNS: usize = 20;
/// Copies of one normalized body in a pair within the window that trip it.
pub const REPEATED_CONTENT: usize = 3;
/// The window of [`REPEATED_CONTENT`]: 30 minutes.
pub const REPEAT_WINDOW_MICROS: i64 = 30 * 60 * 1_000_000;
/// Agent turns in a chain of mail-woken turns above which a message trips
/// its pair.
pub const MAX_THREAD_DEPTH: i64 = 40;
/// How far back a pair's mail and a sender's claims are read when no human
/// prompt was recorded more recently: a project whose prompts are not
/// captured still gets a bounded window instead of its whole history.
pub const GUARD_WINDOW_MICROS: i64 = 2 * 3_600 * 1_000_000;
/// Most pair messages read for one assessment.
const MAX_PAIR_MESSAGES: i64 = 400;

/// One side of a pair: a harness in a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentSide {
    /// Workspace id.
    pub workspace_id: WorkspaceId,
    /// Project id.
    pub project_id: ProjectId,
    /// Harness.
    pub agent: AgentKind,
}

impl AgentSide {
    fn key(&self) -> ([u8; 16], [u8; 16], &'static str) {
        (
            *self.workspace_id.as_bytes(),
            *self.project_id.as_bytes(),
            self.agent.as_str(),
        )
    }
}

/// The two sides in the stored order (side a is the smaller key).
fn ordered(x: AgentSide, y: AgentSide) -> (AgentSide, AgentSide) {
    if x.key() <= y.key() { (x, y) } else { (y, x) }
}

/// Why a pair's breaker opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// [`PING_PONG_TURNS`] direction changes.
    PingPong,
    /// [`REPEATED_CONTENT`] copies of one body.
    RepeatedContent,
    /// A chain deeper than [`MAX_THREAD_DEPTH`].
    ThreadDepth,
}

impl PauseReason {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PingPong => "ping_pong",
            Self::RepeatedContent => "repeated_content",
            Self::ThreadDepth => "thread_depth",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        [Self::PingPong, Self::RepeatedContent, Self::ThreadDepth]
            .into_iter()
            .find(|reason| reason.as_str() == raw)
    }
}

/// A body reduced to what a loop repeats: lowercase, ids and numbers
/// replaced by `#`, whitespace collapsed. Two bodies that differ only in a
/// counter or an id read the same.
#[must_use]
pub fn normalized_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len().min(4_096));
    let mut in_token = false;
    for word in body.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        for c in word.chars() {
            if c.is_ascii_hexdigit() && (c.is_ascii_digit() || in_token) {
                if !in_token {
                    out.push('#');
                    in_token = true;
                }
                continue;
            }
            if c == '-' && in_token {
                continue;
            }
            in_token = false;
            out.extend(c.to_lowercase());
        }
        in_token = false;
        if out.len() > 4_096 {
            break;
        }
    }
    out
}

/// The newest human prompt time of `side`, 0 when none was recorded.
fn anchor(tx: &Transaction<'_>, side: AgentSide) -> StoreResult<i64> {
    Ok(tx
        .query_row(
            "SELECT last_prompt_at FROM agent_human_anchors \
             WHERE workspace_id = ?1 AND project_id = ?2 AND agent_kind = ?3",
            params![
                side.workspace_id.as_bytes(),
                side.project_id.as_bytes(),
                side.agent.as_str()
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// The pair's row: when it was paused (if it is) and when it was last
/// resumed.
fn pair_state(
    tx: &Transaction<'_>,
    a: AgentSide,
    b: AgentSide,
) -> StoreResult<(Option<i64>, Option<i64>)> {
    Ok(tx
        .query_row(
            "SELECT paused_at, resumed_at FROM agent_pair_pauses \
             WHERE a_workspace_id = ?1 AND a_project_id = ?2 AND a_agent = ?3 \
               AND b_workspace_id = ?4 AND b_project_id = ?5 AND b_agent = ?6",
            params![
                a.workspace_id.as_bytes(),
                a.project_id.as_bytes(),
                a.agent.as_str(),
                b.workspace_id.as_bytes(),
                b.project_id.as_bytes(),
                b.agent.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, None)))
}

/// Assess a message about to be inserted from `from` to `to` with `body`,
/// at `now`: return its hop count, and open the pair's breaker when the
/// message completes a loop signal. Mail an agent sends itself, and shared
/// mail with no target harness, is neither counted nor paused.
pub(crate) fn assess_message_in_tx(
    tx: &Transaction<'_>,
    from: AgentSide,
    to: Option<AgentSide>,
    body: &str,
    now: i64,
) -> StoreResult<i64> {
    let window_start = now.saturating_sub(GUARD_WINDOW_MICROS);
    let sender_since = anchor(tx, from)?.max(window_start);
    let woken_by: Option<i64> = tx.query_row(
        "SELECT MAX(hop) FROM agent_messages \
         WHERE to_workspace_id = ?1 AND to_project_id = ?2 AND to_agent = ?3 \
           AND kind = 'message' AND state = 'claimed' AND claimed_at > ?4",
        params![
            from.workspace_id.as_bytes(),
            from.project_id.as_bytes(),
            from.agent.as_str(),
            sender_since
        ],
        |row| row.get(0),
    )?;
    let hop = woken_by.map_or(0, |hop| hop.saturating_add(1));
    let Some(to) = to.filter(|to| *to != from) else {
        return Ok(hop);
    };
    let (a, b) = ordered(from, to);
    let (paused_at, resumed_at) = pair_state(tx, a, b)?;
    if paused_at.is_some() {
        return Ok(hop);
    }
    let since = anchor(tx, a)?
        .max(anchor(tx, b)?)
        .max(resumed_at.unwrap_or(0))
        .max(window_start);
    let reason = if hop > MAX_THREAD_DEPTH {
        Some(PauseReason::ThreadDepth)
    } else {
        pair_signal(tx, from, to, body, since, now)?
    };
    if let Some(reason) = reason {
        pause_pair_in_tx(tx, a, b, reason, now)?;
    }
    Ok(hop)
}

/// The ping-pong or repeated-content signal of the pair's mail since
/// `since`, counting the message being sent.
fn pair_signal(
    tx: &Transaction<'_>,
    from: AgentSide,
    to: AgentSide,
    body: &str,
    since: i64,
    now: i64,
) -> StoreResult<Option<PauseReason>> {
    let mut stmt = tx.prepare_cached(
        "SELECT from_workspace_id = ?1 AND from_project_id = ?2 AND from_agent = ?3, \
                body, created_at \
         FROM agent_messages \
         WHERE kind = 'message' AND created_at > ?7 \
           AND ((from_workspace_id = ?1 AND from_project_id = ?2 AND from_agent = ?3 \
                 AND to_workspace_id = ?4 AND to_project_id = ?5 AND to_agent = ?6) \
             OR (from_workspace_id = ?4 AND from_project_id = ?5 AND from_agent = ?6 \
                 AND to_workspace_id = ?1 AND to_project_id = ?2 AND to_agent = ?3)) \
         ORDER BY created_at DESC LIMIT ?8",
    )?;
    let mut rows: Vec<(bool, String, i64)> = stmt
        .query_map(
            params![
                from.workspace_id.as_bytes(),
                from.project_id.as_bytes(),
                from.agent.as_str(),
                to.workspace_id.as_bytes(),
                to.project_id.as_bytes(),
                to.agent.as_str(),
                since,
                MAX_PAIR_MESSAGES
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<Result<_, _>>()?;
    rows.reverse();
    rows.push((true, body.to_owned(), now));
    let turns = rows
        .windows(2)
        .filter(|pair| pair[0].0 != pair[1].0)
        .count();
    if turns >= PING_PONG_TURNS {
        return Ok(Some(PauseReason::PingPong));
    }
    let wanted = normalized_body(body);
    let recent = now.saturating_sub(REPEAT_WINDOW_MICROS);
    let copies = rows
        .iter()
        .filter(|(_, text, at)| *at >= recent && normalized_body(text) == wanted)
        .count();
    Ok((copies >= REPEATED_CONTENT).then_some(PauseReason::RepeatedContent))
}

fn pause_pair_in_tx(
    tx: &Transaction<'_>,
    a: AgentSide,
    b: AgentSide,
    reason: PauseReason,
    now: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO agent_pair_pauses \
             (a_workspace_id, a_project_id, a_agent, b_workspace_id, b_project_id, b_agent, \
              paused_at, reason, trip_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1) \
         ON CONFLICT DO UPDATE SET paused_at = excluded.paused_at, reason = excluded.reason, \
             trip_count = agent_pair_pauses.trip_count + 1",
        params![
            a.workspace_id.as_bytes(),
            a.project_id.as_bytes(),
            a.agent.as_str(),
            b.workspace_id.as_bytes(),
            b.project_id.as_bytes(),
            b.agent.as_str(),
            now,
            reason.as_str()
        ],
    )?;
    let detail = serde_json::json!({
        "a_agent": a.agent.as_str(),
        "b_agent": b.agent.as_str(),
        "b_workspace_id": b.workspace_id.to_string(),
        "b_project_id": b.project_id.to_string(),
        "reason": reason.as_str(),
    });
    crate::ops::audit_with_detail(
        tx,
        "agent_pair_paused",
        Some(a.workspace_id.as_bytes()),
        Some(a.project_id.as_bytes()),
        None,
        None,
        now,
        &detail.to_string(),
    )
}

/// Record that a human prompted a session of `side` at `at`, and resume
/// every paused pair it is part of.
pub(crate) fn record_human_prompt_in_tx(
    tx: &Transaction<'_>,
    side: AgentSide,
    at: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO agent_human_anchors (workspace_id, project_id, agent_kind, last_prompt_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT DO UPDATE SET last_prompt_at = MAX(last_prompt_at, excluded.last_prompt_at)",
        params![
            side.workspace_id.as_bytes(),
            side.project_id.as_bytes(),
            side.agent.as_str(),
            at
        ],
    )?;
    let resumed = tx.execute(
        "UPDATE agent_pair_pauses SET paused_at = NULL, resumed_at = ?4, \
             resumed_via = 'human_prompt' \
         WHERE paused_at IS NOT NULL AND paused_at <= ?4 \
           AND ((a_workspace_id = ?1 AND a_project_id = ?2 AND a_agent = ?3) \
             OR (b_workspace_id = ?1 AND b_project_id = ?2 AND b_agent = ?3))",
        params![
            side.workspace_id.as_bytes(),
            side.project_id.as_bytes(),
            side.agent.as_str(),
            at
        ],
    )?;
    if resumed > 0 {
        crate::ops::audit_with_detail(
            tx,
            "agent_pair_resumed",
            Some(side.workspace_id.as_bytes()),
            Some(side.project_id.as_bytes()),
            None,
            None,
            at,
            &serde_json::json!({"agent": side.agent.as_str(), "via": "human_prompt"}).to_string(),
        )?;
    }
    Ok(())
}

/// Resume one paused pair from the dashboard. Returns whether it was
/// paused.
pub(crate) fn resume_pair(
    conn: &mut rusqlite::Connection,
    x: AgentSide,
    y: AgentSide,
) -> StoreResult<bool> {
    let (a, b) = ordered(x, y);
    let now = jiff::Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let resumed = tx.execute(
        "UPDATE agent_pair_pauses SET paused_at = NULL, resumed_at = ?7, \
             resumed_via = 'dashboard' \
         WHERE paused_at IS NOT NULL \
           AND a_workspace_id = ?1 AND a_project_id = ?2 AND a_agent = ?3 \
           AND b_workspace_id = ?4 AND b_project_id = ?5 AND b_agent = ?6",
        params![
            a.workspace_id.as_bytes(),
            a.project_id.as_bytes(),
            a.agent.as_str(),
            b.workspace_id.as_bytes(),
            b.project_id.as_bytes(),
            b.agent.as_str(),
            now
        ],
    )? > 0;
    if resumed {
        crate::ops::audit_with_detail(
            &tx,
            "agent_pair_resumed",
            Some(a.workspace_id.as_bytes()),
            Some(a.project_id.as_bytes()),
            None,
            None,
            now,
            &serde_json::json!({
                "a_agent": a.agent.as_str(),
                "b_agent": b.agent.as_str(),
                "via": "dashboard",
            })
            .to_string(),
        )?;
    }
    tx.commit()?;
    Ok(resumed)
}

/// Record, in the audit log, that the host supervisor trusted an adapter's
/// program on first use or trusted a changed binary at the recorded path.
/// `detail` is the supervisor's JSON: adapter id, path and hashes.
pub(crate) fn record_bus_program(conn: &mut rusqlite::Connection, detail: &str) -> StoreResult<()> {
    let tx = conn.transaction()?;
    crate::ops::audit_with_detail(
        &tx,
        "agent_bus_program_trusted",
        None,
        None,
        None,
        None,
        jiff::Timestamp::now().as_microsecond(),
        detail,
    )?;
    tx.commit()?;
    Ok(())
}

/// The newest wake outcome of one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastWake {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Harness.
    pub agent: AgentKind,
    /// `wake_attempted`, `wake_failed`, `agent_unavailable` or
    /// `quota_exhausted`.
    pub event: String,
    /// Its reason, when it has one.
    pub reason: Option<String>,
    /// When, unix microseconds.
    pub at: i64,
}

/// A paused pair, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairPause {
    /// One side, with its workspace and project names.
    pub a: (AgentSide, String, String),
    /// The other side.
    pub b: (AgentSide, String, String),
    /// When the breaker opened, unix microseconds.
    pub paused_at: i64,
    /// Why.
    pub reason: PauseReason,
    /// How many times the pair tripped.
    pub trip_count: i64,
}

impl ReaderPool {
    /// Every paused pair, newest first, at most `limit`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn paused_agent_pairs(&self, limit: usize) -> StoreResult<Vec<PairPause>> {
        let limit = i64::try_from(limit.clamp(1, 1_000)).unwrap_or(1_000);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT p.a_workspace_id, p.a_project_id, p.a_agent, wa.name, pa.name, \
                        p.b_workspace_id, p.b_project_id, p.b_agent, wb.name, pb.name, \
                        p.paused_at, p.reason, p.trip_count \
                 FROM agent_pair_pauses p \
                 JOIN workspaces wa ON wa.id = p.a_workspace_id \
                 JOIN projects pa ON pa.id = p.a_project_id \
                 JOIN workspaces wb ON wb.id = p.b_workspace_id \
                 JOIN projects pb ON pb.id = p.b_project_id \
                 WHERE p.paused_at IS NOT NULL \
                 ORDER BY p.paused_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (aw, ap, aa, awn, apn, bw, bp, ba, bwn, bpn, at, reason, trips) = row?;
                let Some(reason) = PauseReason::parse(&reason) else {
                    continue;
                };
                out.push(PairPause {
                    a: (
                        AgentSide {
                            workspace_id: WorkspaceId::from_slice(&aw)?,
                            project_id: ProjectId::from_slice(&ap)?,
                            agent: AgentKind::from_wire(&aa),
                        },
                        awn,
                        apn,
                    ),
                    b: (
                        AgentSide {
                            workspace_id: WorkspaceId::from_slice(&bw)?,
                            project_id: ProjectId::from_slice(&bp)?,
                            agent: AgentKind::from_wire(&ba),
                        },
                        bwn,
                        bpn,
                    ),
                    paused_at: at,
                    reason,
                    trip_count: trips,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The newest wake outcome reported for each agent, newest first, at
    /// most `limit`.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn last_wakes(&self, limit: usize) -> StoreResult<Vec<LastWake>> {
        let limit = i64::try_from(limit.clamp(1, 1_000)).unwrap_or(1_000);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "WITH last AS ( \
                     SELECT m.to_workspace_id AS ws, m.to_project_id AS pj, m.to_agent AS agent, \
                            MAX(e.id) AS id \
                     FROM message_delivery_events e JOIN agent_messages m ON m.id = e.message_id \
                     WHERE e.event IN ('wake_attempted','wake_failed','agent_unavailable', \
                                       'quota_exhausted') \
                       AND m.to_agent IS NOT NULL \
                     GROUP BY m.to_workspace_id, m.to_project_id, m.to_agent) \
                 SELECT w.name, p.name, last.agent, e.event, e.reason, e.at \
                 FROM last JOIN message_delivery_events e ON e.id = last.id \
                 JOIN workspaces w ON w.id = last.ws JOIN projects p ON p.id = last.pj \
                 ORDER BY e.at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok(LastWake {
                    workspace: row.get(0)?,
                    project: row.get(1)?,
                    agent: AgentKind::from_wire(&row.get::<_, String>(2)?),
                    event: row.get(3)?,
                    reason: row.get(4)?,
                    at: row.get(5)?,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .await
    }

    /// The sides `side` is paused with.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn paused_partners(&self, side: AgentSide) -> StoreResult<Vec<AgentSide>> {
        self.with_conn(move |conn| paused_partners(conn, side))
            .await
    }
}

/// The sides `side` is paused with, read on any connection.
pub(crate) fn paused_partners(
    conn: &rusqlite::Connection,
    side: AgentSide,
) -> StoreResult<Vec<AgentSide>> {
    let mut stmt = conn.prepare_cached(
        "SELECT b_workspace_id, b_project_id, b_agent FROM agent_pair_pauses \
         WHERE paused_at IS NOT NULL AND a_workspace_id = ?1 AND a_project_id = ?2 AND a_agent = ?3 \
         UNION ALL \
         SELECT a_workspace_id, a_project_id, a_agent FROM agent_pair_pauses \
         WHERE paused_at IS NOT NULL AND b_workspace_id = ?1 AND b_project_id = ?2 AND b_agent = ?3",
    )?;
    let rows = stmt.query_map(
        params![
            side.workspace_id.as_bytes(),
            side.project_id.as_bytes(),
            side.agent.as_str()
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (ws, pj, agent) = row?;
        out.push(AgentSide {
            workspace_id: WorkspaceId::from_slice(&ws)?,
            project_id: ProjectId::from_slice(&pj)?,
            agent: AgentKind::from_wire(&agent),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_case_space_ids_and_numbers() {
        assert_eq!(normalized_body("  Hello\n\tWORLD  "), "hello world");
        assert_eq!(
            normalized_body("retry 3 of id 0190f0a4-5c1e-7a2b-9c3d-4e5f60718293"),
            normalized_body("Retry 41 of id 7aa0f0a4-5c1e-7a2b-9c3d-4e5f60718200")
        );
        assert_ne!(normalized_body("ship it"), normalized_body("stop it"));
        assert_eq!(normalized_body("deadbeef"), "deadbeef", "words stay words");
    }
}
