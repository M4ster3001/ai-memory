//! Agent bus regression suite, store layer, mail between the agents of ONE
//! project (`docs/agent-bus/TESTING.md`).
//!
//! What a change to the store must not break: an addressed message reaches its
//! agent and only that agent; a reply closes the cycle with a receipt; a
//! conversation of many rounds stays under the loop guard and the guard trips
//! on exactly its three signals and lifts on a human act; delivery states tell
//! the sender what happened without losing the mail; a pop is idempotent; an
//! expired message is never delivered; and a restart repeats nothing.
//!
//! Clocks: the guard and the sweep read the writer's clock, so a test that
//! needs old mail moves the stored time through a second connection instead
//! of sleeping.

use ai_memory_core::message_wake::{WakeHandleKind, WakeOutcome, wake_text};
use ai_memory_core::{
    AgentKind, DeliveryEventKind, DeliveryReason, DeliveryState, MessageId, MessageKind,
    MessageState, NotifyClass, NotifyOn, ReceiptOutcome,
};
use ai_memory_store::{MAX_THREAD_DEPTH, PING_PONG_TURNS, PauseReason, REPEATED_CONTENT};

use super::agent_bus_regression_support::{
    HOUR_US, MINUTE_US, audit_count, candidates, hop, inbox, linked, new_message, now_us, open,
    outbox_row, paused, pop, project, prompt, raw, send, session_with_handle, side, start_session,
};

const CLAUDE: AgentKind = AgentKind::ClaudeCode;
const CODEX: AgentKind = AgentKind::Codex;
const GEMINI: AgentKind = AgentKind::GeminiCli;
const CODEX_THREAD: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718291";

#[tokio::test]
async fn agent_bus_regression_a_message_to_another_agent_is_pending_for_that_agent_alone() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    start_session(&store, at, CODEX).await;
    start_session(&store, at, CLAUDE).await;
    let id = send(&store, at, CLAUDE, at, CODEX, "BODY-CANARY please review").await;

    let for_codex = candidates(&store, at, CODEX).await;
    assert_eq!(for_codex.len(), 1, "{for_codex:?}");
    let candidate = &for_codex[0];
    assert_eq!(candidate.id, id);
    assert_eq!(candidate.kind, MessageKind::Message);
    assert_eq!(
        (candidate.from_workspace_id, candidate.from_project_id),
        (at.0, at.1),
        "mail inside one project is the same-project case"
    );
    assert_eq!(candidate.from_agent, CLAUDE);
    assert!(!candidate.linked && !candidate.pair_paused);
    // The read the wake gate makes has no field for text.
    assert!(!format!("{candidate:?}").contains("CANARY"));
    assert!(
        candidates(&store, at, CLAUDE).await.is_empty(),
        "the other agent of the project is not the addressee"
    );
    assert_eq!(
        outbox_row(&store, at, id).await.delivery.state,
        DeliveryState::Queued
    );
}

#[tokio::test]
async fn agent_bus_regression_the_bus_lists_agents_with_a_handle_and_ids_only() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    session_with_handle(&store, at, CODEX, CODEX_THREAD, WakeHandleKind::CodexThread).await;
    let addressed = send(&store, at, CLAUDE, at, CODEX, "BODY-CANARY").await;
    // Shared mail (no addressee) and mail for a harness with no handle are
    // not a target: a room is the way to reach every agent.
    store
        .writer
        .insert_message(new_message(at, CLAUDE, at, None, "shared"))
        .await
        .unwrap();
    send(&store, at, CLAUDE, at, GEMINI, "no handle for gemini").await;

    let targets = store.reader.bus_targets(10, 10).await.unwrap();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(targets[0].agent, CODEX);
    assert_eq!(targets[0].handle.handle, CODEX_THREAD);
    assert_eq!(targets[0].pending, vec![addressed]);
    assert!(!targets[0].truncated);
    assert!(!format!("{targets:?}").contains("CANARY"));
}

#[tokio::test]
async fn agent_bus_regression_a_reply_closes_the_cycle_with_a_delivered_receipt_and_the_answer() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    let claude_session = start_session(&store, at, CLAUDE).await;
    start_session(&store, at, CODEX).await;
    let mut question = new_message(at, CLAUDE, at, Some(CODEX), "can you review the diff?");
    question.from_session_id = Some(claude_session);
    question.notify = NotifyOn::from_classes(&[NotifyClass::Success]).unwrap();
    let asked = store.writer.insert_message(question).await.unwrap();

    // Codex pops it: the sender's row is delivered and a receipt waits for
    // the sender, addressed to the harness that asked.
    let popped = pop(&store, at, CODEX, asked).await.unwrap();
    assert_eq!(popped.delivery.state, DeliveryState::Delivered);
    assert_eq!(
        outbox_row(&store, at, asked).await.delivery.state,
        DeliveryState::Delivered
    );
    let waiting = inbox(&store, at).await;
    let receipt = waiting
        .iter()
        .find(|m| m.kind == MessageKind::Receipt)
        .expect("a delivered receipt for the sender");
    let block = receipt.receipt.as_ref().unwrap();
    assert_eq!(block.receipt_for, Some(asked));
    assert_eq!(block.outcome, ReceiptOutcome::Delivered);
    assert_eq!(receipt.to_agent, Some(CLAUDE));
    assert!(!receipt.body.contains("review the diff"));

    // The reply is an ordinary message the other way; the cycle is two
    // messages and one receipt, all settled once read.
    let answered = send(&store, at, CODEX, at, CLAUDE, "looks good, merge it").await;
    assert_eq!(candidates(&store, at, CLAUDE).await.len(), 2);
    assert!(pop(&store, at, CLAUDE, receipt.id).await.is_some());
    let reply = pop(&store, at, CLAUDE, answered).await.unwrap();
    assert_eq!(reply.body, "looks good, merge it");
    assert_eq!(
        outbox_row(&store, at, answered).await.delivery.state,
        DeliveryState::Delivered
    );
    assert!(inbox(&store, at).await.is_empty(), "nothing left waiting");
    assert_eq!(hop(&store, asked), 0);
    assert_eq!(hop(&store, answered), 1, "the reply is one hop deeper");
}

const ROUND_WORDS: [&str; 18] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
];

#[tokio::test]
async fn agent_bus_regression_nine_rounds_of_ping_pong_stay_under_the_guard_and_all_arrive() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    let (mut last, mut delivered) = (None, 0);
    for round in 0..9 {
        let question = send(
            &store,
            at,
            CLAUDE,
            at,
            CODEX,
            &format!("question {}", ROUND_WORDS[2 * round]),
        )
        .await;
        assert!(pop(&store, at, CODEX, question).await.is_some());
        let answer = send(
            &store,
            at,
            CODEX,
            at,
            CLAUDE,
            &format!("answer {}", ROUND_WORDS[2 * round + 1]),
        )
        .await;
        assert!(pop(&store, at, CLAUDE, answer).await.is_some());
        delivered += 2;
        last = Some(answer);
    }
    assert_eq!(delivered, 18, "17 direction changes, one under the limit");
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty(),
        "a long but ordinary conversation is never paused"
    );
    assert_eq!(
        hop(&store, last.unwrap()),
        17,
        "each reply is one hop deeper"
    );
    assert_eq!(store.reader.pending_agent_mail_count().await.unwrap(), 0);
}

#[tokio::test]
async fn agent_bus_regression_twenty_direction_changes_pause_the_pair_and_resume_lifts_it() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    start_session(&store, at, CLAUDE).await;
    start_session(&store, at, CODEX).await;
    // Mail the pair has not read yet is what the guard holds back.
    for turn in 0..PING_PONG_TURNS {
        let (from, to) = if turn % 2 == 0 {
            (CLAUDE, CODEX)
        } else {
            (CODEX, CLAUDE)
        };
        send(
            &store,
            at,
            from,
            at,
            to,
            &format!("step {}", ROUND_WORDS[turn % 18]),
        )
        .await;
    }
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty(),
        "{PING_PONG_TURNS} messages are {} direction changes, one short",
        PING_PONG_TURNS - 1
    );
    let tripping = send(&store, at, CLAUDE, at, CODEX, "one more").await;
    let pauses = store.reader.paused_agent_pairs(10).await.unwrap();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(pauses[0].reason, PauseReason::PingPong);
    assert_eq!(audit_count(&store, "agent_pair_paused").await, 1);
    // The tripping message is still delivered; only wakes between the two stop.
    assert!(paused(&store, at, CLAUDE, CODEX).await);
    assert!(paused(&store, at, CODEX, CLAUDE).await);
    assert_eq!(
        outbox_row(&store, at, tripping).await.delivery.state,
        DeliveryState::Queued
    );
    assert!(pop(&store, at, CODEX, tripping).await.is_some());

    // A third agent still reaches either side.
    send(&store, at, GEMINI, at, CODEX, "an unrelated question").await;
    assert!(!paused(&store, at, GEMINI, CODEX).await);

    // The "Resume" button, in either order of the pair.
    assert!(
        store
            .writer
            .resume_agent_pair(side(at, CODEX), side(at, CLAUDE))
            .await
            .unwrap()
    );
    assert!(
        !store
            .writer
            .resume_agent_pair(side(at, CLAUDE), side(at, CODEX))
            .await
            .unwrap(),
        "resuming twice changes nothing"
    );
    assert_eq!(audit_count(&store, "agent_pair_resumed").await, 1);
    assert!(!paused(&store, at, CODEX, CLAUDE).await);
    // Counting restarts at the resume.
    send(&store, at, CLAUDE, at, CODEX, "after the resume").await;
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn agent_bus_regression_three_equal_bodies_in_thirty_minutes_pause_the_pair_until_a_human_prompt()
 {
    let (_tmp, store) = open();
    let at = project(&store, "web").await;
    assert_eq!(REPEATED_CONTENT, 3);
    // Case, spacing, ids and numbers do not make a body different.
    send(
        &store,
        at,
        CLAUDE,
        at,
        CODEX,
        "Build failed: retry 1 of job 0190f0a4",
    )
    .await;
    send(
        &store,
        at,
        CLAUDE,
        at,
        CODEX,
        "build failed:   retry 2 of job 7f00aa11",
    )
    .await;
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty(),
        "two copies are not a loop"
    );
    send(
        &store,
        at,
        CLAUDE,
        at,
        CODEX,
        "BUILD FAILED: retry 3 of job 12345678",
    )
    .await;
    let pauses = store.reader.paused_agent_pairs(10).await.unwrap();
    assert_eq!(pauses.len(), 1);
    assert_eq!(pauses[0].reason, PauseReason::RepeatedContent);
    assert!(paused(&store, at, CLAUDE, CODEX).await);

    // The bus's own wake text typed as a prompt is not a human.
    let text = wake_text(CODEX, "default", "web", 1, MessageId::new());
    prompt(&store, at, CODEX, "s-codex", &text).await;
    assert!(paused(&store, at, CLAUDE, CODEX).await);
    // A real prompt in a session of either side resumes the pair.
    prompt(&store, at, CODEX, "s-codex", "carry on with the review").await;
    assert!(!paused(&store, at, CLAUDE, CODEX).await);
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(audit_count(&store, "agent_pair_resumed").await, 1);
    // Copies from before the prompt no longer count.
    send(
        &store,
        at,
        CLAUDE,
        at,
        CODEX,
        "build failed: retry 4 of job 1",
    )
    .await;
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn agent_bus_regression_copies_older_than_thirty_minutes_do_not_count() {
    let (_tmp, store) = open();
    let at = project(&store, "web").await;
    let first = send(&store, at, CLAUDE, at, CODEX, "same words again 1").await;
    let second = send(&store, at, CLAUDE, at, CODEX, "same words again 2").await;
    // Move the two earlier copies 31 minutes into the past.
    let conn = raw(&store);
    for id in [first, second] {
        conn.execute(
            "UPDATE agent_messages SET created_at = created_at - ?2 WHERE id = ?1",
            rusqlite::params![&id.as_bytes()[..], 31 * MINUTE_US],
        )
        .unwrap();
    }
    send(&store, at, CLAUDE, at, CODEX, "same words again 3").await;
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty(),
        "the copies fell out of the 30-minute window"
    );
    // One more inside the window makes two in the window, still no loop; a
    // fourth is the third copy in it.
    send(&store, at, CLAUDE, at, CODEX, "same words again 4").await;
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
    send(&store, at, CLAUDE, at, CODEX, "same words again 5").await;
    assert_eq!(store.reader.paused_agent_pairs(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn agent_bus_regression_a_chain_deeper_than_forty_turns_pauses_and_a_prompt_restarts_it() {
    let (_tmp, store) = open();
    let at = project(&store, "ring").await;
    let ring = [CLAUDE, CODEX, GEMINI];
    let depth = usize::try_from(MAX_THREAD_DEPTH).unwrap();
    assert_eq!(depth, 40);
    // Three agents pass the work round; no pair ever changes direction twice
    // in a row, so only the chain depth can see it.
    for hop_no in 0..=depth {
        let (from, to) = (ring[hop_no % 3], ring[(hop_no + 1) % 3]);
        let id = send(
            &store,
            at,
            from,
            at,
            to,
            &format!("hand over {}", "x".repeat(hop_no)),
        )
        .await;
        assert!(
            store
                .reader
                .paused_agent_pairs(10)
                .await
                .unwrap()
                .is_empty(),
            "hop {hop_no} is within the depth"
        );
        assert!(pop(&store, at, to, id).await.is_some());
    }
    let (from, to) = (ring[(depth + 1) % 3], ring[(depth + 2) % 3]);
    send(&store, at, from, at, to, "one hop too deep").await;
    let pauses = store.reader.paused_agent_pairs(10).await.unwrap();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(pauses[0].reason, PauseReason::ThreadDepth);
    assert!(paused(&store, at, from, to).await);

    // A human prompt to the sender starts a new chain at hop zero.
    prompt(&store, at, from, "s-ring", "start over").await;
    let fresh = send(&store, at, from, at, to, "a fresh start").await;
    assert_eq!(hop(&store, fresh), 0);
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn agent_bus_regression_mail_for_an_agent_never_seen_waits_then_queues_on_its_first_session()
{
    let (_tmp, store) = open();
    let a = project(&store, "sender").await;
    let b = project(&store, "inbox").await;
    let id = send(&store, a, CLAUDE, b, CODEX, "hello before you exist").await;
    let row = outbox_row(&store, a, id).await;
    assert_eq!(row.delivery.state, DeliveryState::AwaitingAgent);
    assert_eq!(
        row.delivery.reason,
        Some(DeliveryReason::AgentNotRegistered)
    );
    assert_eq!(
        row.state,
        MessageState::Pending,
        "the mail is stored, not lost"
    );
    // A session of another harness changes nothing.
    start_session(&store, b, CLAUDE).await;
    assert_eq!(
        outbox_row(&store, a, id).await.delivery.state,
        DeliveryState::AwaitingAgent
    );
    start_session(&store, b, CODEX).await;
    let row = outbox_row(&store, a, id).await;
    assert_eq!(row.delivery.state, DeliveryState::Queued);
    assert!(pop(&store, b, CODEX, id).await.is_some());
}

#[tokio::test]
async fn agent_bus_regression_a_failed_wake_is_reported_by_reason_and_the_mail_stays_readable() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    start_session(&store, at, CLAUDE).await;
    start_session(&store, at, CODEX).await;
    let gone = send(&store, at, CLAUDE, at, CODEX, "BODY-CANARY session gone").await;
    let looping = send(&store, at, CLAUDE, at, CODEX, "BODY-CANARY looping").await;

    for (id, reason) in [
        (gone, DeliveryReason::SessionNotFound),
        (looping, DeliveryReason::LoopSuspected),
    ] {
        let report = store
            .writer
            .record_wake_report(at.0, at.1, CODEX, vec![id], WakeOutcome::Failed(reason))
            .await
            .unwrap();
        assert_eq!((report.updated, report.receipts), (1, 1), "{reason:?}");
        let row = outbox_row(&store, at, id).await;
        assert_eq!(row.delivery.state, DeliveryState::WakeFailed);
        assert_eq!(row.delivery.reason, Some(reason));
        assert_eq!(
            row.state,
            MessageState::Pending,
            "a failed wake loses nothing"
        );
        let events = store
            .reader
            .message_delivery_events(at.0, at.1, id)
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.event == DeliveryEventKind::WakeFailed && e.reason == Some(reason)),
            "{events:?}"
        );
    }
    // The same failure reported again records its event but sends no second
    // receipt.
    let again = store
        .writer
        .record_wake_report(
            at.0,
            at.1,
            CODEX,
            vec![gone],
            WakeOutcome::Failed(DeliveryReason::SessionNotFound),
        )
        .await
        .unwrap();
    assert_eq!((again.updated, again.receipts), (1, 0));
    // The receipts name ids and enumerated values, never the body.
    let receipts: Vec<_> = inbox(&store, at)
        .await
        .into_iter()
        .filter(|m| m.kind == MessageKind::Receipt)
        .collect();
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|r| !r.body.contains("CANARY")));

    // Both are still there for the agent to read, then delivered.
    let mut pending: Vec<MessageId> = candidates(&store, at, CODEX)
        .await
        .iter()
        .map(|c| c.id)
        .collect();
    pending.sort_by_key(ToString::to_string);
    let mut expected = vec![gone, looping];
    expected.sort_by_key(ToString::to_string);
    assert_eq!(pending, expected);
    for id in [gone, looping] {
        let popped = pop(&store, at, CODEX, id).await.unwrap();
        assert_eq!(popped.delivery.state, DeliveryState::Delivered);
    }
}

#[tokio::test]
async fn agent_bus_regression_a_pop_is_idempotent_and_settles_the_sender_once() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    let session = start_session(&store, at, CLAUDE).await;
    start_session(&store, at, CODEX).await;
    let mut m = new_message(at, CLAUDE, at, Some(CODEX), "once only");
    m.from_session_id = Some(session);
    m.notify = NotifyOn::from_classes(&[NotifyClass::Success]).unwrap();
    let id = store.writer.insert_message(m).await.unwrap();

    let first = pop(&store, at, CODEX, id).await;
    assert_eq!(first.unwrap().body, "once only");
    // A second pop, by anyone, gets nothing and changes nothing.
    assert!(pop(&store, at, CODEX, id).await.is_none());
    assert!(pop(&store, at, CLAUDE, id).await.is_none());
    assert!(
        store
            .writer
            .pop_message(super::agent_bus_regression_support::claim(at, CODEX), None)
            .await
            .unwrap()
            .is_none(),
        "no id: nothing left to take either"
    );
    let delivered_events = store
        .reader
        .message_delivery_events(at.0, at.1, id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event == DeliveryEventKind::Delivered)
        .count();
    assert_eq!(delivered_events, 1);
    let receipts = inbox(&store, at)
        .await
        .into_iter()
        .filter(|m| m.kind == MessageKind::Receipt)
        .count();
    assert_eq!(receipts, 1, "one receipt, not one per pop");
    assert_eq!(
        store.reader.pending_agent_mail_count().await.unwrap(),
        0,
        "the one message was claimed once"
    );
}

#[tokio::test]
async fn agent_bus_regression_an_expired_message_is_never_delivered_and_the_sender_hears_once() {
    let (_tmp, store) = open();
    let at = project(&store, "api").await;
    start_session(&store, at, CODEX).await;
    let mut m = new_message(at, CLAUDE, at, Some(CODEX), "short lived");
    m.ttl_secs = Some(60);
    let id = store.writer.insert_message(m).await.unwrap();

    let early = store
        .writer
        .sweep_message_deadlines(now_us(), 100)
        .await
        .unwrap();
    assert_eq!(early.expired, 0, "not before its time");
    let later = now_us() + HOUR_US;
    let swept = store
        .writer
        .sweep_message_deadlines(later, 100)
        .await
        .unwrap();
    assert_eq!((swept.expired, swept.receipts), (1, 1));
    let row = outbox_row(&store, at, id).await;
    assert_eq!(row.state, MessageState::Expired);
    assert_eq!(row.delivery.state, DeliveryState::Expired);
    assert_eq!(row.delivery.reason, Some(DeliveryReason::Ttl));
    assert!(
        pop(&store, at, CODEX, id).await.is_none(),
        "never delivered"
    );
    assert!(candidates(&store, at, CODEX).await.is_empty());
    // A second sweep repeats nothing.
    let again = store
        .writer
        .sweep_message_deadlines(later + HOUR_US, 100)
        .await
        .unwrap();
    assert_eq!((again.expired, again.receipts), (0, 0));
}

#[tokio::test]
async fn agent_bus_regression_a_restart_repeats_no_mail_no_wake_report_and_no_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = ai_memory_store::Store::open(tmp.path()).unwrap();
    let a = project(&store, "api").await;
    let b = project(&store, "web").await;
    let claude = start_session(&store, a, CLAUDE).await;
    start_session(&store, b, CODEX).await;
    let mut wanted = new_message(a, CLAUDE, b, Some(CODEX), "wake me once");
    wanted.from_session_id = Some(claude);
    let woken = store.writer.insert_message(wanted).await.unwrap();
    let mut popped = new_message(a, CLAUDE, b, Some(CODEX), "read before the restart");
    popped.notify = NotifyOn::from_classes(&[NotifyClass::Success]).unwrap();
    let read = store.writer.insert_message(popped).await.unwrap();
    store
        .writer
        .record_wake_report(b.0, b.1, CODEX, vec![woken], WakeOutcome::Attempted)
        .await
        .unwrap();
    assert!(pop(&store, b, CODEX, read).await.is_some());
    // Trip a breaker so a restart has one to remember.
    for n in 0..3 {
        send(&store, b, CODEX, b, CLAUDE, &format!("same loop {n}")).await;
    }
    assert_eq!(store.reader.paused_agent_pairs(10).await.unwrap().len(), 1);
    let receipts_before = inbox(&store, a).await.len();

    store.close().await;
    drop(store);
    store = ai_memory_store::Store::open(tmp.path()).unwrap();

    // The mail that was waiting still waits, once; the popped one is gone.
    let waiting: Vec<MessageId> = candidates(&store, b, CODEX)
        .await
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(waiting, vec![woken]);
    assert!(
        pop(&store, b, CODEX, read).await.is_none(),
        "no second delivery"
    );
    assert_eq!(
        outbox_row(&store, a, woken).await.delivery.state,
        DeliveryState::WakeAttempted,
        "the wake that was reported is still on record"
    );
    assert_eq!(
        outbox_row(&store, a, read).await.delivery.state,
        DeliveryState::Delivered
    );
    assert_eq!(
        inbox(&store, a).await.len(),
        receipts_before,
        "no receipt was repeated"
    );
    assert_eq!(
        store.reader.paused_agent_pairs(10).await.unwrap().len(),
        1,
        "a paused pair stays paused across a restart"
    );
    // A supervisor that repeats its report after the restart changes nothing
    // that matters: no duplicate receipt, the message is still one row.
    let sweep = store
        .writer
        .sweep_message_deadlines(now_us(), 100)
        .await
        .unwrap();
    assert_eq!(sweep.receipts, 0);
    assert!(
        !linked(&store, b, CODEX, a).await,
        "no link appeared from nowhere"
    );
}
