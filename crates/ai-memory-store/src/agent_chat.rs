//! The read behind the root-only `/web/chat` page: one project's
//! conversation, merged from addressed mail between agents and room posts,
//! with the delivery state of each mail and its receipts folded under it.
//!
//! Text is returned only where the project may show it: mail inside the
//! project, mail between this project and another one while a link between
//! the two was active when it was sent, and room posts of the project. Mail
//! with any other project comes back without its subject and body, only with
//! who and where, so the page can show that it happened and never what it
//! said.

use ai_memory_core::{AgentKind, MessageId};
use rusqlite::{Connection, params};

use crate::StoreResult;
use crate::agent_links::{LinkWindow, ProjectKey, link_windows};
use crate::reader::ReaderPool;

/// Most items one read returns per kind.
pub const CHAT_ITEM_LIMIT: usize = 150;

/// Where the other end of a mail is, relative to the project shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatProject {
    /// Ids.
    pub key: ProjectKey,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
}

/// Why a mail's text is or is not returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatScope {
    /// Both ends are the project shown.
    Internal,
    /// The other end is another project, and a link between the two was
    /// active when the mail was sent.
    Linked,
    /// The other end is another project with no link then: no text.
    Hidden,
}

/// A delivery receipt folded under the mail it reports on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReceipt {
    /// When the receipt was written, unix microseconds.
    pub at: i64,
    /// `delivered`, `delayed`, `wake_failed`, ...
    pub outcome: String,
    /// The reason word, when there is one.
    pub reason: Option<String>,
}

/// One mail between agents (or from the root user).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMail {
    /// The message id.
    pub id: MessageId,
    /// When it was sent, unix microseconds.
    pub at: i64,
    /// The sender harness; `Other` is the web composer (the human).
    pub from_agent: AgentKind,
    /// The sender's project.
    pub from: ChatProject,
    /// The addressed harness, `None` for mail any agent of the project reads.
    pub to_agent: Option<AgentKind>,
    /// The recipient's project.
    pub to: ChatProject,
    /// Whether the text is returned.
    pub scope: ChatScope,
    /// The subject, only when the text is returned.
    pub subject: Option<String>,
    /// The body, only when the text is returned.
    pub body: Option<String>,
    /// `pending`, `claimed`, `cancelled`, `editing` or `expired`.
    pub state: String,
    /// `queued`, `wake_attempted`, `delivered`, `wake_failed`, ...
    pub delivery_state: String,
    /// The reason word of the delivery state.
    pub delivery_reason: Option<String>,
    /// Receipts, oldest first.
    pub receipts: Vec<ChatReceipt>,
}

/// One room post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatPost {
    /// When, unix microseconds.
    pub at: i64,
    /// The room.
    pub room: String,
    /// Position in the room.
    pub seq: i64,
    /// The poster; `Other` is the human.
    pub from_agent: AgentKind,
    /// The text.
    pub body: String,
    /// The harnesses it mentioned.
    pub mentions: Vec<AgentKind>,
}

/// An item of the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatItem {
    /// Addressed mail.
    Mail(Box<ChatMail>),
    /// A room post.
    Post(ChatPost),
}

impl ChatItem {
    /// When the item happened.
    #[must_use]
    pub fn at(&self) -> i64 {
        match self {
            Self::Mail(mail) => mail.at,
            Self::Post(post) => post.at,
        }
    }
}

fn scope_of(shown: ProjectKey, other: ProjectKey, at: i64, windows: &[LinkWindow]) -> ChatScope {
    if other.project_id == shown.project_id {
        ChatScope::Internal
    } else if windows
        .iter()
        .any(|w| w.other == other.project_id && w.from <= at && at <= w.until)
    {
        ChatScope::Linked
    } else {
        ChatScope::Hidden
    }
}

fn timeline(conn: &Connection, project: ProjectKey, limit: i64) -> StoreResult<Vec<ChatItem>> {
    let windows = link_windows(conn, project.project_id)?;
    let ws = project.workspace_id.as_bytes();
    let pj = project.project_id.as_bytes();
    let mut items: Vec<ChatItem> = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT m.id, m.created_at, m.from_agent, m.from_workspace_id, m.from_project_id, \
                fw.name, fp.name, m.to_agent, m.to_workspace_id, m.to_project_id, tw.name, tp.name, \
                m.subject, m.body, m.state, m.delivery_state, m.delivery_reason \
         FROM agent_messages m \
         JOIN workspaces fw ON fw.id = m.from_workspace_id \
         JOIN projects fp ON fp.id = m.from_project_id \
         JOIN workspaces tw ON tw.id = m.to_workspace_id \
         JOIN projects tp ON tp.id = m.to_project_id \
         WHERE m.kind = 'message' \
           AND ((m.from_workspace_id = ?1 AND m.from_project_id = ?2) \
             OR (m.to_workspace_id = ?1 AND m.to_project_id = ?2)) \
         ORDER BY m.created_at DESC, m.id DESC LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![ws, pj, limit])?;
    while let Some(row) = rows.next()? {
        let from = ChatProject {
            key: ProjectKey {
                workspace_id: ai_memory_core::WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(3)?)?,
                project_id: ai_memory_core::ProjectId::from_slice(&row.get::<_, Vec<u8>>(4)?)?,
            },
            workspace: row.get(5)?,
            project: row.get(6)?,
        };
        let to = ChatProject {
            key: ProjectKey {
                workspace_id: ai_memory_core::WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(8)?)?,
                project_id: ai_memory_core::ProjectId::from_slice(&row.get::<_, Vec<u8>>(9)?)?,
            },
            workspace: row.get(10)?,
            project: row.get(11)?,
        };
        let at: i64 = row.get(1)?;
        let other = if from.key.project_id == project.project_id {
            to.key
        } else {
            from.key
        };
        let scope = scope_of(project, other, at, &windows);
        let shown = scope != ChatScope::Hidden;
        items.push(ChatItem::Mail(Box::new(ChatMail {
            id: MessageId::from_slice(&row.get::<_, Vec<u8>>(0)?)?,
            at,
            from_agent: AgentKind::from_wire(&row.get::<_, String>(2)?),
            from,
            to_agent: row
                .get::<_, Option<String>>(7)?
                .map(|agent| AgentKind::from_wire(&agent)),
            to,
            scope,
            subject: if shown { row.get(12)? } else { None },
            body: if shown { Some(row.get(13)?) } else { None },
            state: row.get(14)?,
            delivery_state: row.get(15)?,
            delivery_reason: row.get(16)?,
            receipts: Vec::new(),
        })));
    }
    drop(rows);
    drop(stmt);

    let mut receipts = conn.prepare(
        "SELECT r.receipt_for, r.created_at, r.receipt_outcome, r.receipt_reason \
         FROM agent_messages r \
         WHERE r.kind = 'receipt' AND r.receipt_for IN ( \
             SELECT m.id FROM agent_messages m WHERE m.kind = 'message' \
               AND ((m.from_workspace_id = ?1 AND m.from_project_id = ?2) \
                 OR (m.to_workspace_id = ?1 AND m.to_project_id = ?2)) \
             ORDER BY m.created_at DESC, m.id DESC LIMIT ?3) \
         ORDER BY r.created_at, r.id",
    )?;
    let rows = receipts.query_map(params![ws, pj, limit], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            ChatReceipt {
                at: row.get(1)?,
                outcome: row.get(2)?,
                reason: row.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (id, receipt) = row?;
        for item in &mut items {
            if let ChatItem::Mail(mail) = item
                && mail.id.as_bytes().as_slice() == id.as_slice()
            {
                mail.receipts.push(receipt.clone());
            }
        }
    }

    let mut posts = conn.prepare(
        "SELECT p.created_at, r.name, p.seq, p.from_agent, p.body, p.mentions \
         FROM agent_room_posts p JOIN agent_rooms r ON r.id = p.room_id \
         WHERE r.workspace_id = ?1 AND r.project_id = ?2 \
         ORDER BY p.created_at DESC, p.seq DESC LIMIT ?3",
    )?;
    let rows = posts.query_map(params![ws, pj, limit], |row| {
        let mentions: String = row.get(5)?;
        Ok(ChatPost {
            at: row.get(0)?,
            room: row.get(1)?,
            seq: row.get(2)?,
            from_agent: AgentKind::from_wire(&row.get::<_, String>(3)?),
            body: row.get(4)?,
            mentions: mentions
                .split(',')
                .filter(|name| !name.is_empty())
                .map(AgentKind::from_wire)
                .collect(),
        })
    })?;
    for row in rows {
        items.push(ChatItem::Post(row?));
    }
    items.sort_by_key(ChatItem::at);
    let keep = usize::try_from(limit).unwrap_or(usize::MAX);
    if items.len() > keep {
        items.drain(..items.len() - keep);
    }
    Ok(items)
}

fn fingerprint(conn: &Connection, project: ProjectKey, at: i64) -> StoreResult<String> {
    let ws = project.workspace_id.as_bytes();
    let pj = project.project_id.as_bytes();
    let parts: [i64; 8] = conn.query_row(
        "SELECT \
           (SELECT COALESCE(MAX(id), 0) FROM message_delivery_events), \
           (SELECT COUNT(*) FROM agent_messages \
             WHERE (from_workspace_id = ?1 AND from_project_id = ?2) \
                OR (to_workspace_id = ?1 AND to_project_id = ?2)), \
           (SELECT COUNT(*) FROM agent_messages \
             WHERE state = 'pending' AND ((from_workspace_id = ?1 AND from_project_id = ?2) \
                OR (to_workspace_id = ?1 AND to_project_id = ?2))), \
           (SELECT COALESCE(SUM(last_seq), 0) FROM agent_rooms \
             WHERE workspace_id = ?1 AND project_id = ?2), \
           (SELECT COUNT(*) FROM agent_link_events e JOIN agent_links l ON l.id = e.link_id \
             WHERE l.from_project_id = ?2 OR l.to_project_id = ?2), \
           (SELECT COUNT(*) FROM agent_links \
             WHERE state IN ('requested','active') AND expires_at > ?3 \
               AND (from_project_id = ?2 OR to_project_id = ?2)), \
           (SELECT COUNT(*) FROM agent_pair_pauses \
             WHERE paused_at IS NOT NULL AND (a_project_id = ?2 OR b_project_id = ?2)), \
           (SELECT COALESCE(MAX(resumed_at), 0) FROM agent_pair_pauses \
             WHERE a_project_id = ?2 OR b_project_id = ?2)",
        params![ws, pj, at],
        |row| {
            Ok([
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ])
        },
    )?;
    Ok(parts
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join("."))
}

impl ReaderPool {
    /// One project's conversation, oldest first: mail between agents and
    /// room posts merged by time, at most `limit` items (capped at
    /// [`CHAT_ITEM_LIMIT`]).
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn chat_timeline(
        &self,
        project: ProjectKey,
        limit: usize,
    ) -> StoreResult<Vec<ChatItem>> {
        let limit = i64::try_from(limit.clamp(1, CHAT_ITEM_LIMIT)).unwrap_or(150);
        self.with_conn(move |conn| timeline(conn, project, limit))
            .await
    }

    /// A short string that changes whenever something the timeline or its
    /// banners show changes (mail, delivery events, posts, links, pauses).
    /// The long-poll compares it; it carries no text.
    ///
    /// # Errors
    /// Propagates any SQL or pool error.
    pub async fn chat_fingerprint(&self, project: ProjectKey) -> StoreResult<String> {
        self.with_conn(move |conn| {
            fingerprint(conn, project, jiff::Timestamp::now().as_microsecond())
        })
        .await
    }
}
