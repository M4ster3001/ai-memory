//! Delivery state, agent registration, deadline sweep and receipts for agent
//! messages (V102, V103). Every function here runs inside a transaction the
//! single writer already holds, so a state change, its timeline event and any
//! receipt it causes commit together.
//!
//! A receipt is built from columns of the original row, a fixed template and
//! enumerated values. No text from the recipient is read, and a receipt row
//! can never ask for a receipt (the V103 CHECK repeats that rule).

use ai_memory_core::{
    AgentKind, DeliveryEventKind, DeliveryReason, MessageId, NotifyOn, ProjectId, ReceiptOutcome,
    WorkspaceId,
    message_delivery::{RECEIPT_TTL_SECS, receipt_body, receipt_subject},
};
use jiff::Timestamp;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::{StoreError, StoreResult};

/// Largest batch one sweep pass may process, whatever the caller asks.
pub const MAX_SWEEP_BATCH: usize = 1_000;

/// A recipient inbox that gained a claimable row, so the writer can wake its
/// watchers after the commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NewInboxRow {
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub to_agent: Option<AgentKind>,
}

/// What one deadline sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeadlineSweepOutcome {
    /// Messages moved to `expired`.
    pub expired: usize,
    /// "Not popped yet" alerts recorded.
    pub alerts: usize,
    /// Receipts queued in a sender's inbox.
    pub receipts: usize,
    /// Receipts dropped because the sender's inbox was full.
    pub receipts_dropped: usize,
    /// Inboxes that gained a receipt (for watcher signals).
    pub(crate) new_rows: Vec<NewInboxRow>,
}

/// Options for listing one side of a mailbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageListFilter {
    /// List every state instead of pending mail only, newest first.
    pub include_settled: bool,
    /// Only this message.
    pub message_id: Option<MessageId>,
}

/// One entry of a message's delivery timeline.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeliveryEvent {
    /// When it happened.
    pub at: Timestamp,
    /// What happened.
    pub event: DeliveryEventKind,
    /// Why, when recorded.
    pub reason: Option<DeliveryReason>,
}

pub(crate) fn record_event(
    tx: &Transaction<'_>,
    message_id: &[u8],
    at: i64,
    event: DeliveryEventKind,
    reason: Option<DeliveryReason>,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO message_delivery_events (message_id, at, event, reason) \
         VALUES (?1, ?2, ?3, ?4)",
        params![message_id, at, event.as_str(), reason.map(|r| r.as_str())],
    )?;
    Ok(())
}

/// Register `agent` in a project the first time a session of it starts there,
/// and move mail that was waiting for it to `queued`. Called right after the
/// session row is inserted, in the same transaction.
pub(crate) fn register_agent_in_tx(
    tx: &Transaction<'_>,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    agent: AgentKind,
    at: i64,
) -> StoreResult<()> {
    let inserted = tx.execute(
        "INSERT INTO agent_registrations (workspace_id, project_id, agent_kind, source, created_at) \
         VALUES (?1, ?2, ?3, 'session', ?4) \
         ON CONFLICT(workspace_id, project_id, agent_kind) DO NOTHING",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            agent.as_str(),
            at
        ],
    )?;
    if inserted == 0 {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO message_delivery_events (message_id, at, event, reason) \
         SELECT id, ?4, 'agent_registered', NULL FROM agent_messages \
         WHERE to_workspace_id = ?1 AND to_project_id = ?2 AND to_agent = ?3 \
           AND delivery_state = 'awaiting_agent'",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            agent.as_str(),
            at
        ],
    )?;
    tx.execute(
        "UPDATE agent_messages SET delivery_state = 'queued', delivery_reason = NULL \
         WHERE to_workspace_id = ?1 AND to_project_id = ?2 AND to_agent = ?3 \
           AND delivery_state = 'awaiting_agent'",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            agent.as_str()
        ],
    )?;
    Ok(())
}

/// Whether `agent` is registered in a project.
pub(crate) fn agent_is_registered(
    tx: &Transaction<'_>,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    agent: AgentKind,
) -> StoreResult<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM agent_registrations \
             WHERE workspace_id = ?1 AND project_id = ?2 AND agent_kind = ?3",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                agent.as_str()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The columns of an original message that a receipt is built from.
struct ReceiptSource {
    id: Vec<u8>,
    kind: String,
    notify: String,
    from_workspace_id: Vec<u8>,
    from_project_id: Vec<u8>,
    from_agent: String,
    sender_session_recorded: bool,
    to_workspace_id: Vec<u8>,
    to_project_id: Vec<u8>,
}

const RECEIPT_SOURCE_COLUMNS: &str = "id, kind, notify, from_workspace_id, from_project_id, \
     from_agent, from_session_id IS NOT NULL, to_workspace_id, to_project_id";

fn receipt_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiptSource> {
    Ok(ReceiptSource {
        id: row.get(0)?,
        kind: row.get(1)?,
        notify: row.get(2)?,
        from_workspace_id: row.get(3)?,
        from_project_id: row.get(4)?,
        from_agent: row.get(5)?,
        sender_session_recorded: row.get(6)?,
        to_workspace_id: row.get(7)?,
        to_project_id: row.get(8)?,
    })
}

/// What [`queue_receipt`] did.
enum ReceiptResult {
    /// Queued; this inbox gained a row.
    Queued(NewInboxRow),
    /// Not wanted, or already sent for this outcome.
    Skipped,
    /// The sender's inbox was full; the drop is recorded on the original.
    Dropped,
}

/// Queue a receipt about `source` in its sender's inbox, when the sender asked
/// for this outcome and none exists for it yet.
fn queue_receipt(
    tx: &Transaction<'_>,
    source: &ReceiptSource,
    outcome: ReceiptOutcome,
    reason: Option<DeliveryReason>,
    at: i64,
) -> StoreResult<ReceiptResult> {
    // A receipt is never the subject of a receipt.
    if source.kind != "message" {
        return Ok(ReceiptResult::Skipped);
    }
    let notify: NotifyOn = source.notify.parse().map_err(StoreError::from)?;
    if !notify.wants(outcome.notify_class()) {
        return Ok(ReceiptResult::Skipped);
    }
    let exists = tx
        .query_row(
            "SELECT 1 FROM agent_messages \
             WHERE kind = 'receipt' AND receipt_for = ?1 AND receipt_outcome = ?2",
            params![source.id, outcome.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(ReceiptResult::Skipped);
    }
    let pending: i64 = tx.query_row(
        "SELECT COUNT(*) FROM agent_messages \
         WHERE to_workspace_id = ?1 AND to_project_id = ?2 AND state = 'pending'",
        params![source.from_workspace_id, source.from_project_id],
        |row| row.get(0),
    )?;
    if u64::try_from(pending).unwrap_or(u64::MAX) >= crate::ops::MAX_PENDING_INBOX_MESSAGES {
        record_event(
            tx,
            &source.id,
            at,
            DeliveryEventKind::ReceiptDropped,
            reason,
        )?;
        return Ok(ReceiptResult::Dropped);
    }
    let original = MessageId::from_slice(&source.id)?;
    let at_ts = Timestamp::from_microsecond(at).map_err(|e| {
        StoreError::Memory(ai_memory_core::MemoryError::MalformedRecord(format!(
            "bad receipt time: {e}"
        )))
    })?;
    // Address the sender's harness only when the server recorded its session;
    // a declared or unknown sender gets the receipt as shared project mail.
    let to_agent = source
        .sender_session_recorded
        .then(|| AgentKind::from_wire(&source.from_agent));
    let id = MessageId::new();
    let expires_at = at.saturating_add(
        i64::try_from(RECEIPT_TTL_SECS.saturating_mul(1_000_000)).unwrap_or(i64::MAX),
    );
    tx.execute(
        "INSERT INTO agent_messages ( \
             id, from_workspace_id, from_project_id, from_agent, from_session_id, \
             from_owner_user, to_workspace_id, to_project_id, subject, body, state, created_at, \
             nature, evidence, to_agent, kind, priority, notify, expires_at, delivery_state, \
             receipt_for, receipt_outcome, receipt_reason \
         ) VALUES (?1, ?2, ?3, 'other', NULL, NULL, ?4, ?5, ?6, ?7, 'pending', ?8, \
             'unknown', NULL, ?9, 'receipt', 'normal', 'never', ?10, 'queued', ?11, ?12, ?13)",
        params![
            id.as_bytes(),
            source.to_workspace_id,
            source.to_project_id,
            source.from_workspace_id,
            source.from_project_id,
            receipt_subject(outcome, original),
            receipt_body(outcome, reason, original, at_ts),
            at,
            to_agent.map(AgentKind::as_str),
            expires_at,
            source.id,
            outcome.as_str(),
            reason.map(|r| r.as_str()),
        ],
    )?;
    record_event(tx, &source.id, at, DeliveryEventKind::ReceiptSent, reason)?;
    Ok(ReceiptResult::Queued(NewInboxRow {
        workspace_id: WorkspaceId::from_slice(&source.from_workspace_id)?,
        project_id: ProjectId::from_slice(&source.from_project_id)?,
        to_agent,
    }))
}

/// After a successful claim: mark the message delivered, record it, and queue
/// a `delivered` receipt when the sender asked for `success`.
pub(crate) fn mark_delivered(
    tx: &Transaction<'_>,
    message_id: &MessageId,
    at: i64,
) -> StoreResult<Option<NewInboxRow>> {
    tx.execute(
        "UPDATE agent_messages SET delivery_state = 'delivered', delivery_reason = NULL, \
             no_pop_alert_at = NULL \
         WHERE id = ?1 AND state = 'claimed'",
        params![message_id.as_bytes()],
    )?;
    record_event(
        tx,
        message_id.as_bytes(),
        at,
        DeliveryEventKind::Delivered,
        None,
    )?;
    let source = tx.query_row(
        &format!("SELECT {RECEIPT_SOURCE_COLUMNS} FROM agent_messages WHERE id = ?1"),
        params![message_id.as_bytes()],
        receipt_source,
    )?;
    Ok(
        match queue_receipt(tx, &source, ReceiptOutcome::Delivered, None, at)? {
            ReceiptResult::Queued(row) => Some(row),
            ReceiptResult::Skipped | ReceiptResult::Dropped => None,
        },
    )
}

/// Expire pending messages whose time to live passed, then record the
/// "not popped yet" alerts that are due, at most `batch` rows in total and
/// oldest deadline first. Each change is a compare-and-set on
/// `state = 'pending'`, so a message popped or cancelled in the meantime is
/// left alone.
pub(crate) fn sweep_message_deadlines(
    tx: &Transaction<'_>,
    now: i64,
    batch: usize,
) -> StoreResult<DeadlineSweepOutcome> {
    let batch = batch.clamp(1, MAX_SWEEP_BATCH);
    let mut outcome = DeadlineSweepOutcome::default();
    let due = |sql: &str, limit: usize| -> StoreResult<Vec<ReceiptSource>> {
        let mut stmt = tx.prepare(sql)?;
        let rows = stmt.query_map(
            params![now, i64::try_from(limit).unwrap_or(i64::MAX)],
            receipt_source,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    };
    let expiring = due(
        &format!(
            "SELECT {RECEIPT_SOURCE_COLUMNS} FROM agent_messages \
             WHERE state = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?1 \
             ORDER BY expires_at ASC, id ASC LIMIT ?2"
        ),
        batch,
    )?;
    for source in &expiring {
        let changed = tx.execute(
            "UPDATE agent_messages SET state = 'expired', delivery_state = 'expired', \
                 delivery_reason = 'ttl', no_pop_alert_at = NULL \
             WHERE id = ?1 AND state = 'pending'",
            params![source.id],
        )?;
        if changed == 0 {
            continue;
        }
        outcome.expired += 1;
        record_event(
            tx,
            &source.id,
            now,
            DeliveryEventKind::Expired,
            Some(DeliveryReason::Ttl),
        )?;
        tally_receipt(
            &mut outcome,
            tx,
            source,
            ReceiptOutcome::Expired,
            Some(DeliveryReason::Ttl),
            now,
        )?;
    }
    let remaining = batch.saturating_sub(expiring.len());
    if remaining == 0 {
        return Ok(outcome);
    }
    let alerting = due(
        &format!(
            "SELECT {RECEIPT_SOURCE_COLUMNS} FROM agent_messages \
             WHERE state = 'pending' AND no_pop_alert_at IS NOT NULL AND no_pop_alert_at <= ?1 \
             ORDER BY no_pop_alert_at ASC, id ASC LIMIT ?2"
        ),
        remaining,
    )?;
    for source in &alerting {
        // The alert fires once: clearing the column is the compare-and-set.
        // Mail still waiting for an unregistered harness keeps that reason.
        let changed = tx.execute(
            "UPDATE agent_messages SET no_pop_alert_at = NULL, \
                 delivery_reason = COALESCE(delivery_reason, 'no_pop_timeout') \
             WHERE id = ?1 AND state = 'pending' AND no_pop_alert_at IS NOT NULL",
            params![source.id],
        )?;
        if changed == 0 {
            continue;
        }
        outcome.alerts += 1;
        record_event(
            tx,
            &source.id,
            now,
            DeliveryEventKind::NoPopAlert,
            Some(DeliveryReason::NoPopTimeout),
        )?;
        tally_receipt(
            &mut outcome,
            tx,
            source,
            ReceiptOutcome::Delayed,
            Some(DeliveryReason::NoPopTimeout),
            now,
        )?;
    }
    Ok(outcome)
}

fn tally_receipt(
    outcome: &mut DeadlineSweepOutcome,
    tx: &Transaction<'_>,
    source: &ReceiptSource,
    receipt: ReceiptOutcome,
    reason: Option<DeliveryReason>,
    now: i64,
) -> StoreResult<()> {
    match queue_receipt(tx, source, receipt, reason, now)? {
        ReceiptResult::Queued(row) => {
            outcome.receipts += 1;
            outcome.new_rows.push(row);
        }
        ReceiptResult::Dropped => outcome.receipts_dropped += 1,
        ReceiptResult::Skipped => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema already refuses a receipt that asks for receipts, so this
    /// checks the code guard on its own: a receipt row is never the subject
    /// of a receipt, whatever its `notify` says.
    #[test]
    fn a_receipt_is_never_the_subject_of_a_receipt() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let tx = conn.transaction().unwrap();
        let source = ReceiptSource {
            id: MessageId::new().as_bytes().to_vec(),
            kind: "receipt".to_owned(),
            notify: "failure,delay,success".to_owned(),
            from_workspace_id: vec![0; 16],
            from_project_id: vec![0; 16],
            from_agent: "codex".to_owned(),
            sender_session_recorded: false,
            to_workspace_id: vec![0; 16],
            to_project_id: vec![0; 16],
        };
        for outcome in ReceiptOutcome::ALL {
            assert!(matches!(
                queue_receipt(&tx, &source, *outcome, None, 1).unwrap(),
                ReceiptResult::Skipped
            ));
        }
    }
}
