//! Delivery states, the agent registry, the deadline sweep, wake reports and
//! receipts (V102 to V104).
//!
//! The guards pinned here: a receipt carries nothing the recipient wrote and
//! never produces another receipt; at most one receipt per message and
//! outcome; a full sender inbox drops the receipt instead of growing; the sweep
//! is bounded; a recipient cannot retract or list the receipts about it; and
//! a delivery timeline is readable only from the sender's own outbox.

use ai_memory_core::{
    AgentKind, DeliveryEventKind, DeliveryReason, DeliveryState, MessageBox, MessageClaim,
    MessageKind, MessagePriority, MessageState, NewAgentMessage, NewSession, NotifyClass, NotifyOn,
    ProjectId, ReceiptOutcome, SessionId, WorkspaceId,
};
use ai_memory_store::{MAX_PENDING_INBOX_MESSAGES, MessageListFilter, Store};

const HOUR_US: i64 = 3_600_000_000;
const DAY_US: i64 = 24 * HOUR_US;

async fn project(store: &Store, name: &str) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, name.to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

async fn start_session(store: &Store, at: (WorkspaceId, ProjectId), agent: AgentKind) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id,
            workspace_id: at.0,
            project_id: at.1,
            agent_kind: agent,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    id
}

fn message(
    from: (WorkspaceId, ProjectId),
    to: (WorkspaceId, ProjectId),
    body: &str,
) -> NewAgentMessage {
    NewAgentMessage {
        from_workspace_id: from.0,
        from_project_id: from.1,
        from_agent: AgentKind::Codex,
        from_session_id: None,
        from_owner_user: None,
        to_workspace_id: to.0,
        to_project_id: to.1,
        to_agent: None,
        subject: Some("sender subject".into()),
        body: body.into(),
        nature: ai_memory_core::MessageNature::Unknown,
        evidence: None,
        priority: MessagePriority::Normal,
        notify: NotifyOn::DEFAULT,
        ttl_secs: None,
    }
}

fn claim(to: (WorkspaceId, ProjectId), agent: AgentKind) -> MessageClaim {
    MessageClaim {
        workspace_id: to.0,
        project_id: to.1,
        claiming_agent: agent,
        claiming_session: None,
        claiming_user: None,
    }
}

async fn outbox_row(
    store: &Store,
    from: (WorkspaceId, ProjectId),
    id: ai_memory_core::MessageId,
) -> ai_memory_core::AgentMessage {
    let rows = store
        .reader
        .list_messages_filtered(
            from.0,
            from.1,
            MessageBox::Outbox,
            MessageListFilter {
                include_settled: true,
                message_id: Some(id),
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly the requested outbox row");
    rows.into_iter().next().unwrap()
}

async fn inbox(store: &Store, at: (WorkspaceId, ProjectId)) -> Vec<ai_memory_core::AgentMessage> {
    store
        .reader
        .list_messages(at.0, at.1, MessageBox::Inbox, 200)
        .await
        .unwrap()
}

fn now_us() -> i64 {
    jiff::Timestamp::now().as_microsecond()
}

#[tokio::test]
async fn mail_for_an_unregistered_harness_waits_until_its_first_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let mut to_codex = message(a, b, "for codex");
    to_codex.to_agent = Some(AgentKind::Codex);
    let id = store.writer.insert_message(to_codex).await.unwrap();

    let row = outbox_row(&store, a, id).await;
    assert_eq!(row.delivery.state, DeliveryState::AwaitingAgent);
    assert_eq!(
        row.delivery.reason,
        Some(DeliveryReason::AgentNotRegistered)
    );
    assert!(row.delivery.expires_at.is_some(), "awaiting mail has a TTL");

    // Another harness starting there changes nothing.
    start_session(&store, b, AgentKind::ClaudeCode).await;
    assert_eq!(
        outbox_row(&store, a, id).await.delivery.state,
        DeliveryState::AwaitingAgent
    );
    // A Codex session in a third project changes nothing either.
    let c = project(&store, "elsewhere").await;
    start_session(&store, c, AgentKind::Codex).await;
    assert_eq!(
        outbox_row(&store, a, id).await.delivery.state,
        DeliveryState::AwaitingAgent
    );

    start_session(&store, b, AgentKind::Codex).await;
    let row = outbox_row(&store, a, id).await;
    assert_eq!(row.delivery.state, DeliveryState::Queued);
    assert_eq!(row.delivery.reason, None);
    let events = store
        .reader
        .message_delivery_events(a.0, a.1, id)
        .await
        .unwrap();
    assert_eq!(
        events.iter().map(|e| e.event).collect::<Vec<_>>(),
        vec![DeliveryEventKind::AgentRegistered]
    );

    // Mail to a registered harness is queued from the start.
    let mut again = message(a, b, "again");
    again.to_agent = Some(AgentKind::Codex);
    let again = store.writer.insert_message(again).await.unwrap();
    assert_eq!(
        outbox_row(&store, a, again).await.delivery.state,
        DeliveryState::Queued
    );
}

#[tokio::test]
async fn a_pop_marks_delivered_and_a_success_receipt_never_bounces() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let sender_session = start_session(&store, a, AgentKind::Codex).await;
    let mut m = message(a, b, "SECRET-SENDER-BODY");
    m.notify = NotifyOn::from_classes(&[NotifyClass::Success]).unwrap();
    m.from_session_id = Some(sender_session);
    let id = store.writer.insert_message(m).await.unwrap();

    let popped = store
        .writer
        .pop_message(claim(b, AgentKind::ClaudeCode), Some(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(popped.delivery.state, DeliveryState::Delivered);
    assert_eq!(popped.kind, MessageKind::Message);

    let receipts = inbox(&store, a).await;
    assert_eq!(receipts.len(), 1);
    let receipt = &receipts[0];
    assert_eq!(receipt.kind, MessageKind::Receipt);
    // A verified sender session addresses the receipt to that harness.
    assert_eq!(receipt.to_agent, Some(AgentKind::Codex));
    assert_eq!(receipt.origin.from_project_id, b.1);
    assert_eq!(receipt.origin.from_agent, AgentKind::Other);
    let block = receipt.receipt.as_ref().unwrap();
    assert_eq!(block.receipt_for, Some(id));
    assert_eq!(block.outcome, ReceiptOutcome::Delivered);
    assert!(!receipt.body.contains("SECRET-SENDER-BODY"));
    assert!(
        !receipt
            .subject
            .as_deref()
            .unwrap()
            .contains("sender subject")
    );
    assert_eq!(receipt.delivery.notify, NotifyOn::NEVER);

    // Popping the receipt produces no receipt for it.
    let popped_receipt = store
        .writer
        .pop_message(claim(a, AgentKind::Codex), Some(receipt.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(popped_receipt.kind, MessageKind::Receipt);
    assert!(inbox(&store, b).await.is_empty());
    assert!(inbox(&store, a).await.is_empty());
}

#[tokio::test]
async fn expiry_bounces_once_and_a_receipt_expires_silently() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let mut m = message(a, b, "short-lived");
    m.ttl_secs = Some(60);
    let id = store.writer.insert_message(m).await.unwrap();

    // Before the deadline nothing happens.
    let early = store
        .writer
        .sweep_message_deadlines(now_us(), 100)
        .await
        .unwrap();
    assert_eq!((early.expired, early.receipts), (0, 0));

    let later = now_us() + HOUR_US;
    let swept = store
        .writer
        .sweep_message_deadlines(later, 100)
        .await
        .unwrap();
    assert_eq!((swept.expired, swept.receipts), (1, 1));
    let row = outbox_row(&store, a, id).await;
    assert_eq!(row.state, MessageState::Expired);
    assert_eq!(row.delivery.state, DeliveryState::Expired);
    assert_eq!(row.delivery.reason, Some(DeliveryReason::Ttl));
    // The expired message can no longer be popped.
    assert!(
        store
            .writer
            .pop_message(claim(b, AgentKind::Codex), Some(id))
            .await
            .unwrap()
            .is_none()
    );
    let receipts = inbox(&store, a).await;
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].receipt.as_ref().unwrap().outcome,
        ReceiptOutcome::Expired
    );
    // Unverified sender: the receipt is shared project mail.
    assert_eq!(receipts[0].to_agent, None);

    // Sweeping again sends nothing new; the receipt itself expires after
    // seven days without generating a receipt.
    let again = store
        .writer
        .sweep_message_deadlines(later + HOUR_US, 100)
        .await
        .unwrap();
    assert_eq!((again.expired, again.receipts), (0, 0));
    let much_later = later + 8 * DAY_US;
    let receipt_expiry = store
        .writer
        .sweep_message_deadlines(much_later, 100)
        .await
        .unwrap();
    assert_eq!((receipt_expiry.expired, receipt_expiry.receipts), (1, 0));
    assert!(inbox(&store, a).await.is_empty());
    assert!(inbox(&store, b).await.is_empty());
}

#[tokio::test]
async fn the_no_pop_alert_fires_once_and_keeps_the_message_queued() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let mut m = message(a, b, "urgent");
    m.priority = MessagePriority::High;
    let id = store.writer.insert_message(m).await.unwrap();
    let alert_at = outbox_row(&store, a, id)
        .await
        .delivery
        .no_pop_alert_at
        .expect("high priority has an alert")
        .as_microsecond();

    let swept = store
        .writer
        .sweep_message_deadlines(alert_at, 100)
        .await
        .unwrap();
    assert_eq!((swept.alerts, swept.receipts, swept.expired), (1, 1, 0));
    let row = outbox_row(&store, a, id).await;
    assert_eq!(row.state, MessageState::Pending);
    assert_eq!(row.delivery.state, DeliveryState::Queued);
    assert_eq!(row.delivery.reason, Some(DeliveryReason::NoPopTimeout));
    assert_eq!(row.delivery.no_pop_alert_at, None);
    let again = store
        .writer
        .sweep_message_deadlines(alert_at + 60_000_000, 100)
        .await
        .unwrap();
    assert_eq!((again.alerts, again.receipts), (0, 0));
    let receipts = inbox(&store, a).await;
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].receipt.as_ref().unwrap().outcome,
        ReceiptOutcome::Delayed
    );

    // Still claimable after the alert.
    assert!(
        store
            .writer
            .pop_message(claim(b, AgentKind::Codex), Some(id))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn notify_never_and_low_priority_send_no_receipts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let mut quiet = message(a, b, "quiet");
    quiet.notify = NotifyOn::NEVER;
    quiet.ttl_secs = Some(60);
    store.writer.insert_message(quiet).await.unwrap();
    let mut low = message(a, b, "low");
    low.priority = MessagePriority::Low;
    let low = store.writer.insert_message(low).await.unwrap();
    assert_eq!(
        outbox_row(&store, a, low).await.delivery.no_pop_alert_at,
        None
    );
    let swept = store
        .writer
        .sweep_message_deadlines(now_us() + 2 * DAY_US, 100)
        .await
        .unwrap();
    assert_eq!((swept.expired, swept.receipts), (1, 0));
    assert!(inbox(&store, a).await.is_empty());
}

#[tokio::test]
async fn a_full_sender_inbox_drops_the_receipt_and_records_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let c = project(&store, "flooder").await;
    let mut m = message(a, b, "will expire");
    m.ttl_secs = Some(60);
    let id = store.writer.insert_message(m).await.unwrap();
    for i in 0..MAX_PENDING_INBOX_MESSAGES {
        let mut filler = message(c, a, &format!("filler {i}"));
        filler.notify = NotifyOn::NEVER;
        store.writer.insert_message(filler).await.unwrap();
    }
    let swept = store
        .writer
        .sweep_message_deadlines(now_us() + HOUR_US, 100)
        .await
        .unwrap();
    assert_eq!(
        (swept.expired, swept.receipts, swept.receipts_dropped),
        (1, 0, 1)
    );
    assert_eq!(
        store.reader.pending_message_count(a.0, a.1).await.unwrap(),
        MAX_PENDING_INBOX_MESSAGES
    );
    let events = store
        .reader
        .message_delivery_events(a.0, a.1, id)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.event == DeliveryEventKind::ReceiptDropped)
    );
}

#[tokio::test]
async fn the_sweep_processes_at_most_one_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    for i in 0..5 {
        let mut m = message(a, b, &format!("m{i}"));
        m.ttl_secs = Some(60);
        m.notify = NotifyOn::NEVER;
        store.writer.insert_message(m).await.unwrap();
    }
    let later = now_us() + HOUR_US;
    let first = store
        .writer
        .sweep_message_deadlines(later, 2)
        .await
        .unwrap();
    assert_eq!(first.expired, 2);
    let rest = store
        .writer
        .sweep_message_deadlines(later, 100)
        .await
        .unwrap();
    assert_eq!(rest.expired, 3);
    // An absurd batch is capped rather than trusted.
    let none_left = store
        .writer
        .sweep_message_deadlines(later, usize::MAX)
        .await
        .unwrap();
    assert_eq!(none_left.expired, 0);
}

/// Receipts carry the recipient project as their sender coordinate. The
/// recipient must not be able to retract them, edit them or see them as its
/// own outbound mail; the real sender's history and timeline stay private to
/// it.
#[tokio::test]
async fn a_recipient_cannot_cancel_or_list_the_receipts_about_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    let mut m = message(a, b, "will expire");
    m.ttl_secs = Some(60);
    let id = store.writer.insert_message(m).await.unwrap();
    store
        .writer
        .sweep_message_deadlines(now_us() + HOUR_US, 100)
        .await
        .unwrap();
    let receipt = inbox(&store, a).await.remove(0);

    // The recipient's outbox does not list it, and cancelling everything
    // there leaves it pending.
    let recipient_outbox = store
        .reader
        .list_messages_filtered(
            b.0,
            b.1,
            MessageBox::Outbox,
            MessageListFilter {
                include_settled: true,
                message_id: None,
            },
            50,
        )
        .await
        .unwrap();
    assert!(recipient_outbox.is_empty(), "{recipient_outbox:?}");
    assert_eq!(
        store.writer.cancel_messages(b.0, b.1, None).await.unwrap(),
        0
    );
    assert_eq!(
        store
            .writer
            .cancel_messages(b.0, b.1, Some(receipt.id))
            .await
            .unwrap(),
        0
    );
    // The web edit hold cannot take a receipt out of the queue either.
    assert!(
        store
            .writer
            .begin_message_edit(a.0, a.1, receipt.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(inbox(&store, a).await.len(), 1);

    // Control: the real sender can cancel its own message.
    let mut own = message(a, b, "mine");
    own.notify = NotifyOn::NEVER;
    let own = store.writer.insert_message(own).await.unwrap();
    assert_eq!(
        store
            .writer
            .cancel_messages(a.0, a.1, Some(own))
            .await
            .unwrap(),
        1
    );
    let cancelled = outbox_row(&store, a, own).await;
    assert_eq!(cancelled.delivery.state, DeliveryState::Cancelled);
    assert_eq!(
        cancelled.delivery.reason,
        Some(DeliveryReason::SenderCancelled)
    );

    // The timeline is the sender's: from the recipient project it is empty.
    assert!(
        !store
            .reader
            .message_delivery_events(a.0, a.1, id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .reader
            .message_delivery_events(b.0, b.1, id)
            .await
            .unwrap()
            .is_empty()
    );
    // A message id from another outbox is not listed either.
    assert!(
        store
            .reader
            .list_messages_filtered(
                b.0,
                b.1,
                MessageBox::Outbox,
                MessageListFilter {
                    include_settled: true,
                    message_id: Some(id),
                },
                10,
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_requested_ttl_outside_the_bounds_is_refused_at_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    for ttl in [
        0,
        59,
        ai_memory_core::message_delivery::MAX_MESSAGE_TTL_SECS + 1,
    ] {
        let mut m = message(a, b, "bad ttl");
        m.ttl_secs = Some(ttl);
        assert!(store.writer.insert_message(m).await.is_err(), "{ttl}");
    }
    // A long TTL is accepted but never extends past the priority default.
    let mut m = message(a, b, "long ttl");
    m.ttl_secs = Some(ai_memory_core::message_delivery::MAX_MESSAGE_TTL_SECS);
    let id = store.writer.insert_message(m).await.unwrap();
    let row = outbox_row(&store, a, id).await;
    let ttl = row.delivery.expires_at.unwrap().as_microsecond() - row.created_at.as_microsecond();
    assert!(ttl <= 3 * DAY_US + 1_000_000, "{ttl}");
}

#[tokio::test]
async fn the_configured_policy_stamps_new_mail() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "recipient").await;
    store
        .writer
        .set_message_delivery_policy(ai_memory_core::DeliveryPolicy {
            ttl_low_secs: 600,
            ttl_normal_secs: 300,
            ttl_high_secs: 120,
            alert_normal_secs: None,
            alert_high_secs: Some(60),
        })
        .await
        .unwrap();
    let id = store
        .writer
        .insert_message(message(a, b, "configured"))
        .await
        .unwrap();
    let row = outbox_row(&store, a, id).await;
    let ttl = row.delivery.expires_at.unwrap().as_microsecond() - row.created_at.as_microsecond();
    assert!((299_000_000..=301_000_000).contains(&ttl), "{ttl}");
    assert_eq!(row.delivery.no_pop_alert_at, None);
}

/// A wake report changes only pending mail in the reported inbox that the
/// reported harness may claim: a popped message keeps `delivered`, and ids
/// from another inbox or for another harness are untouched. A failure sends
/// the sender one receipt per outcome; a quota report carries `retry_after`
/// on the message and on its receipt.
#[tokio::test]
async fn wake_reports_touch_only_claimable_mail_and_bounce_once() {
    use ai_memory_core::message_wake::WakeOutcome;
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "inbox").await;
    let c = project(&store, "elsewhere").await;
    start_session(&store, b, AgentKind::Codex).await;
    start_session(&store, b, AgentKind::ClaudeCode).await;
    let send = |to, agent: Option<AgentKind>| {
        let mut m = message(a, to, "BODY-CANARY");
        m.to_agent = agent;
        let writer = store.writer.clone();
        async move { writer.insert_message(m).await.unwrap() }
    };
    let mine = send(b, Some(AgentKind::Codex)).await;
    let shared = send(b, None).await;
    let other_harness = send(b, Some(AgentKind::ClaudeCode)).await;
    let other_inbox = send(c, Some(AgentKind::Codex)).await;
    let popped = send(b, Some(AgentKind::Codex)).await;
    store
        .writer
        .pop_message(claim(b, AgentKind::Codex), Some(popped))
        .await
        .unwrap();

    let report = store
        .writer
        .record_wake_report(
            b.0,
            b.1,
            AgentKind::Codex,
            vec![mine, shared, other_harness, other_inbox, popped, mine],
            WakeOutcome::Attempted,
        )
        .await
        .unwrap();
    assert_eq!(report.updated, 2, "{report:?}");
    assert_eq!(report.receipts, 0, "an attempted wake sends no receipt");
    for (id, state) in [
        (mine, DeliveryState::WakeAttempted),
        (shared, DeliveryState::WakeAttempted),
        (other_harness, DeliveryState::Queued),
        (other_inbox, DeliveryState::AwaitingAgent),
        (popped, DeliveryState::Delivered),
    ] {
        assert_eq!(outbox_row(&store, a, id).await.delivery.state, state);
    }

    // Quota: state, reason and reset time on the message and its receipt.
    let quota = WakeOutcome::QuotaExhausted {
        retry_after_secs: Some(3_600),
    };
    let before = jiff::Timestamp::now();
    let report = store
        .writer
        .record_wake_report(b.0, b.1, AgentKind::Codex, vec![mine], quota)
        .await
        .unwrap();
    assert_eq!((report.updated, report.receipts), (1, 1));
    let row = outbox_row(&store, a, mine).await;
    assert_eq!(row.delivery.state, DeliveryState::QuotaExhausted);
    assert_eq!(row.delivery.reason, Some(DeliveryReason::QuotaMarker));
    let retry = row.delivery.retry_after.unwrap();
    assert!(
        retry >= before + jiff::SignedDuration::from_secs(3_590),
        "{retry}"
    );
    // The same outcome again records the event but sends no second receipt.
    let report = store
        .writer
        .record_wake_report(b.0, b.1, AgentKind::Codex, vec![mine], quota)
        .await
        .unwrap();
    assert_eq!((report.updated, report.receipts), (1, 0));
    let receipts: Vec<_> = inbox(&store, a)
        .await
        .into_iter()
        .filter(|m| m.kind == MessageKind::Receipt)
        .collect();
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    let receipt = receipts[0].receipt.as_ref().unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::QuotaExhausted);
    assert_eq!(receipt.reason, Some(DeliveryReason::QuotaMarker));
    assert_eq!(receipt.retry_after, Some(retry));
    assert!(!receipts[0].body.contains("CANARY"));

    // A budget refusal is a different outcome: one more receipt.
    let report = store
        .writer
        .record_wake_report(
            b.0,
            b.1,
            AgentKind::Codex,
            vec![mine],
            WakeOutcome::Failed(DeliveryReason::BudgetExhausted),
        )
        .await
        .unwrap();
    assert_eq!((report.updated, report.receipts), (1, 1));
    let events: Vec<DeliveryEventKind> = store
        .reader
        .message_delivery_events(a.0, a.1, mine)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert_eq!(
        events,
        [
            DeliveryEventKind::WakeAttempted,
            DeliveryEventKind::QuotaExhausted,
            DeliveryEventKind::ReceiptSent,
            DeliveryEventKind::QuotaExhausted,
            DeliveryEventKind::WakeFailed,
            DeliveryEventKind::ReceiptSent,
        ]
    );
    // The other inbox's message is out of reach whatever the report says.
    let report = store
        .writer
        .record_wake_report(
            b.0,
            b.1,
            AgentKind::Codex,
            vec![other_inbox],
            WakeOutcome::Failed(DeliveryReason::AdapterExitNonzero),
        )
        .await
        .unwrap();
    assert_eq!((report.updated, report.receipts), (0, 0));
}

/// The wake candidates name kind and sender provenance only, for the
/// harness's claimable mail.
#[tokio::test]
async fn wake_candidates_report_kind_and_recorded_sender_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "sender").await;
    let b = project(&store, "inbox").await;
    let session = start_session(&store, a, AgentKind::ClaudeCode).await;
    start_session(&store, b, AgentKind::Codex).await;
    let mut verified = message(a, b, "x");
    verified.to_agent = Some(AgentKind::Codex);
    verified.from_session_id = Some(session);
    let verified = store.writer.insert_message(verified).await.unwrap();
    let mut named = message(a, b, "y");
    named.to_agent = Some(AgentKind::Codex);
    let named = store.writer.insert_message(named).await.unwrap();
    let shared = store
        .writer
        .insert_message(message(a, b, "z"))
        .await
        .unwrap();

    let only_mine = store
        .reader
        .wake_candidates(b.0, b.1, AgentKind::Codex, false, 100)
        .await
        .unwrap();
    let ids: Vec<_> = only_mine.iter().map(|c| c.id).collect();
    assert_eq!(ids, [verified, named]);
    assert!(only_mine[0].sender_session_recorded);
    assert!(!only_mine[1].sender_session_recorded);
    assert_eq!(only_mine[0].from_project_id, a.1);
    assert_eq!(only_mine[0].from_agent, AgentKind::Codex);
    assert_eq!(only_mine[0].kind, MessageKind::Message);
    let with_shared = store
        .reader
        .wake_candidates(b.0, b.1, AgentKind::Codex, true, 100)
        .await
        .unwrap();
    assert_eq!(with_shared.len(), 3);
    assert_eq!(with_shared[2].id, shared);
    assert!(
        store
            .reader
            .wake_candidates(b.0, b.1, AgentKind::ClaudeCode, false, 100)
            .await
            .unwrap()
            .is_empty()
    );
}
