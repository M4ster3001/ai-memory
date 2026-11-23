//! Agent bus regression suite, store layer, mail BETWEEN projects with and
//! without a link (`docs/agent-bus/TESTING.md`).
//!
//! The rules a change must not weaken: mail to another project is always
//! delivered but wakes nobody until a link joins the two; a request is inert;
//! only the destination confirms; a link wakes exactly its two projects, in
//! both directions, until its own clock runs out (the sweep is not needed);
//! closing, purging and the limits end or bound it; the chat may show the text
//! of such mail only for mail sent while the link was active; and everything
//! is in the audit trail.
//!
//! The link clock is read through `wake_candidates_at`, so no test sleeps for
//! an expiry.

use ai_memory_core::agent_links::{
    ConfirmRefusal, LinkState, MAX_AGENT_REQUESTS_PER_HOUR, MAX_LIVE_LINKS_PER_PROJECT,
};
use ai_memory_core::{AgentKind, DeliveryState};
use ai_memory_store::{
    AgentConfirm, ChatItem, ChatScope, LinkCloseOutcome, LinkCloser, LinkConfirmOutcome,
    LinkRequestOutcome, RequestRefusal,
};

use super::agent_bus_regression_support::{
    HOUR_US, agent_request, audit_count, candidates, created, human_request, key, linked,
    linked_at, now_us, open, outbox_row, pop, project, prompt, raw, send, tick,
};

const CLAUDE: AgentKind = AgentKind::ClaudeCode;
const CODEX: AgentKind = AgentKind::Codex;

#[tokio::test]
async fn agent_bus_regression_without_a_link_mail_is_delivered_but_does_not_wake() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = send(&store, a, CLAUDE, b, CODEX, "BODY-CANARY across projects").await;

    // It is in the recipient's inbox and the sender can see where it stands.
    let found = candidates(&store, b, CODEX).await;
    assert_eq!(found.len(), 1);
    assert_ne!(found[0].from_project_id, b.1, "another project");
    assert!(!found[0].linked, "no link: nothing marks it as wakeable");
    assert_eq!(
        outbox_row(&store, a, id).await.delivery.state,
        DeliveryState::AwaitingAgent,
        "stored for an agent not seen there yet"
    );
    // It is read the way mail always is: by the recipient, on its own.
    let popped = pop(&store, b, CODEX, id).await.unwrap();
    assert_eq!(popped.body, "BODY-CANARY across projects");
    assert_eq!(
        outbox_row(&store, a, id).await.delivery.state,
        DeliveryState::Delivered
    );
}

#[tokio::test]
async fn agent_bus_regression_a_request_is_inert_and_wakes_nobody() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    send(&store, a, CLAUDE, b, CODEX, "from a").await;
    send(&store, b, CODEX, a, CLAUDE, "from b").await;

    for request in [
        human_request(a, b, false),
        agent_request(a, CLAUDE, "s-a", b),
    ] {
        let id = created(store.writer.link_request(request).await.unwrap());
        let view = store.reader.agent_link(id).await.unwrap().unwrap();
        assert_eq!(view.state, LinkState::Requested);
        assert!(!linked(&store, b, CODEX, a).await, "{view:?}");
        assert!(!linked(&store, a, CLAUDE, b).await, "{view:?}");
        store
            .writer
            .link_close(id, LinkCloser::Human)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn agent_bus_regression_only_the_destination_confirms() {
    let (_tmp, store) = open();
    let (a, b, c) = (
        project(&store, "api").await,
        project(&store, "web").await,
        project(&store, "other").await,
    );
    send(&store, a, CLAUDE, b, CODEX, "waiting").await;
    // The requester's own agent asked, after its user said so.
    prompt(&store, a, CLAUDE, "s-a", "connect with the web project").await;
    let id = created(
        store
            .writer
            .link_request(agent_request(a, CLAUDE, "s-a", b))
            .await
            .unwrap(),
    );
    let refused = |outcome: LinkConfirmOutcome| match outcome {
        LinkConfirmOutcome::Refused(refusal) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let as_agent = |project, agent, session: Option<&str>| AgentConfirm {
        link: id,
        project: key(project),
        agent,
        session: session.map(ai_memory_core::SessionId::from_native),
    };

    // The wrong web page: the requester's, and a third project's.
    for page in [a, c] {
        assert_eq!(
            refused(store.writer.link_confirm_web(id, key(page)).await.unwrap()),
            ConfirmRefusal::NotDestination
        );
    }
    // The requester's agent never confirms its own request.
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(as_agent(a, CLAUDE, Some("s-a")))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NotDestination
    );
    // The destination's agent must have been shown the request...
    prompt(&store, b, CODEX, "s-b", "yes, go ahead").await;
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(as_agent(b, CODEX, Some("s-b")))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NotPresented
    );
    // ...and its user must answer after that, not before.
    tick().await;
    let shown = store
        .writer
        .link_present(key(b), CODEX, ai_memory_core::SessionId::from_native("s-b"))
        .await
        .unwrap();
    assert_eq!(shown, vec![id]);
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(as_agent(b, CODEX, Some("s-b")))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NoUserAnswer
    );
    assert!(
        !linked(&store, b, CODEX, a).await,
        "every refusal left the link inert"
    );
    tick().await;
    prompt(&store, b, CODEX, "s-b", "yes, connect them").await;
    assert!(matches!(
        store
            .writer
            .link_confirm_agent(as_agent(b, CODEX, Some("s-b")))
            .await
            .unwrap(),
        LinkConfirmOutcome::Confirmed { .. }
    ));
    assert!(linked(&store, b, CODEX, a).await);
    let view = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!(view.confirmed_via.as_deref(), Some("agent"));

    // The trail shows the refusals by code.
    let events = store.reader.agent_link_events(id, 50).await.unwrap();
    let refusals: Vec<&str> = events
        .iter()
        .filter(|(_, event, _, _)| event == "confirm_refused")
        .map(|(_, _, _, detail)| detail.as_str())
        .collect();
    for code in ["not_destination", "not_presented", "no_user_answer"] {
        assert!(refusals.contains(&code), "{code} missing in {refusals:?}");
    }
}

#[tokio::test]
async fn agent_bus_regression_a_session_the_server_never_recorded_cannot_confirm() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, false))
            .await
            .unwrap(),
    );
    let outcome = store
        .writer
        .link_confirm_agent(AgentConfirm {
            link: id,
            project: key(b),
            agent: CODEX,
            session: None,
        })
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LinkConfirmOutcome::Refused(ConfirmRefusal::NoSession)
    );
    // And a confirmed link cannot be confirmed again to move its clock.
    store.writer.link_confirm_web(id, key(b)).await.unwrap();
    assert_eq!(
        store.writer.link_confirm_web(id, key(b)).await.unwrap(),
        LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested)
    );
}

#[tokio::test]
async fn agent_bus_regression_an_active_link_wakes_both_ways_and_only_for_its_pair() {
    let (_tmp, store) = open();
    let (a, b, c) = (
        project(&store, "api").await,
        project(&store, "web").await,
        project(&store, "other").await,
    );
    send(&store, a, CLAUDE, b, CODEX, "a to b").await;
    send(&store, b, CODEX, a, CLAUDE, "b to a").await;
    send(&store, c, CLAUDE, b, CODEX, "c to b").await;
    created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    assert!(linked(&store, b, CODEX, a).await);
    assert!(
        linked(&store, a, CLAUDE, b).await,
        "the effect is symmetric"
    );
    assert!(
        !linked(&store, b, CODEX, c).await,
        "a third project stays closed"
    );
}

#[tokio::test]
async fn agent_bus_regression_a_link_is_symmetric_in_effect_but_directional_in_the_record() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    let view = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!(view.from.key, key(a), "the record keeps who asked");
    assert_eq!(view.to.key, key(b), "and who confirms");
    assert_eq!(view.other_side(a.1).key, key(b));
    assert_eq!(view.other_side(b.1).key, key(a));
    // Asking from the other side finds the same link and does not reverse it.
    assert_eq!(
        store
            .writer
            .link_request(human_request(b, a, false))
            .await
            .unwrap(),
        LinkRequestOutcome::Existing(id, LinkState::Active)
    );
    let again = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!((again.from.key, again.to.key), (key(a), key(b)));
}

#[tokio::test]
async fn agent_bus_regression_one_pair_has_one_live_link_in_either_direction() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, false))
            .await
            .unwrap(),
    );
    for (from, to) in [(a, b), (b, a)] {
        assert_eq!(
            store
                .writer
                .link_request(human_request(from, to, true))
                .await
                .unwrap(),
            LinkRequestOutcome::Existing(id, LinkState::Requested),
            "asking again, or from the other side, cannot skip the confirmation"
        );
    }
    assert_eq!(
        store
            .writer
            .link_request(human_request(a, a, false))
            .await
            .unwrap(),
        LinkRequestOutcome::Refused(RequestRefusal::SameProject)
    );
    assert_eq!(
        store
            .reader
            .agent_links(None, false, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // Once it ended, the pair may ask again and gets a new link.
    store
        .writer
        .link_close(id, LinkCloser::Human)
        .await
        .unwrap();
    let next = created(
        store
            .writer
            .link_request(human_request(b, a, false))
            .await
            .unwrap(),
    );
    assert_ne!(next, id);
}

#[tokio::test]
async fn agent_bus_regression_the_link_clock_ends_waking_with_no_sweep() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    send(&store, a, CLAUDE, b, CODEX, "hello").await;
    send(&store, b, CODEX, a, CLAUDE, "hello back").await;
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    let ends = store
        .reader
        .agent_link(id)
        .await
        .unwrap()
        .unwrap()
        .expires_at;
    assert!(
        ends - now_us() > 2 * HOUR_US - 60_000_000,
        "validity counts from the confirmation: two hours by default"
    );
    // One microsecond before the end the link still wakes; at the end it is
    // over. The sweep has not run in between.
    assert!(linked_at(&store, b, CODEX, a, ends - 1).await);
    assert!(!linked_at(&store, b, CODEX, a, ends).await);
    assert!(!linked_at(&store, a, CLAUDE, b, ends + HOUR_US).await);
    let view = store
        .reader
        .agent_links_at(None, true, 10, ends)
        .await
        .unwrap();
    assert_eq!(view[0].state, LinkState::Expired, "read as expired at once");
    // The stored row still says active until the sweep ends it for real.
    assert_eq!(
        store.reader.agent_link(id).await.unwrap().unwrap().state,
        LinkState::Active
    );
    store
        .writer
        .sweep_message_deadlines(ends, 10)
        .await
        .unwrap();
    let stored = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!(stored.state, LinkState::Expired);
    assert_eq!(stored.close_reason.as_deref(), Some("expired"));
}

#[tokio::test]
async fn agent_bus_regression_an_unconfirmed_request_expires_after_a_day() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, false))
            .await
            .unwrap(),
    );
    let ends = store
        .reader
        .agent_link(id)
        .await
        .unwrap()
        .unwrap()
        .expires_at;
    assert!(ends - now_us() > 23 * HOUR_US, "24 hours to be answered");
    let late = store
        .reader
        .agent_links_at(None, true, 10, ends)
        .await
        .unwrap();
    assert_eq!(late[0].state, LinkState::Expired);
    store
        .writer
        .sweep_message_deadlines(ends, 10)
        .await
        .unwrap();
    assert_eq!(
        store.writer.link_confirm_web(id, key(b)).await.unwrap(),
        LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested),
        "an expired request cannot be confirmed"
    );
}

#[tokio::test]
async fn agent_bus_regression_closing_ends_waking_at_once_and_a_third_project_cannot_close() {
    let (_tmp, store) = open();
    let (a, b, c) = (
        project(&store, "api").await,
        project(&store, "web").await,
        project(&store, "other").await,
    );
    send(&store, a, CLAUDE, b, CODEX, "hi").await;
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    assert_eq!(
        store
            .writer
            .link_close(
                id,
                LinkCloser::Agent {
                    project: key(c),
                    agent: CODEX
                }
            )
            .await
            .unwrap(),
        LinkCloseOutcome::NotAllowed
    );
    assert!(linked(&store, b, CODEX, a).await, "still active");
    // Either side may close: here the requester's agent.
    assert_eq!(
        store
            .writer
            .link_close(
                id,
                LinkCloser::Agent {
                    project: key(a),
                    agent: CLAUDE
                }
            )
            .await
            .unwrap(),
        LinkCloseOutcome::Closed(LinkState::Active)
    );
    assert!(!linked(&store, b, CODEX, a).await);
    assert_eq!(
        store
            .writer
            .link_close(id, LinkCloser::Human)
            .await
            .unwrap(),
        LinkCloseOutcome::NotLive
    );
}

#[tokio::test]
async fn agent_bus_regression_purging_a_project_ends_its_links_in_the_audit_log() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    store
        .writer
        .purge_project(
            b.0,
            b.1,
            "default/web",
            None,
            true,
            ai_memory_store::Compaction::Skip,
            ai_memory_store::PurgeMode::Commit,
        )
        .await
        .unwrap();
    assert!(store.reader.agent_link(id).await.unwrap().is_none());
    assert!(
        store
            .reader
            .agent_links(None, true, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing live is left for the purged project"
    );
    let audit = store
        .reader
        .list_audit_events(ai_memory_store::AuditLogFilter {
            op: Some("agent_link_closed".to_owned()),
            limit: 10,
            ..ai_memory_store::AuditLogFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].detail.contains("project_purged"));
    assert!(audit[0].detail.contains(&id.to_string()));
}

#[tokio::test]
async fn agent_bus_regression_a_project_holds_eight_live_links_and_asks_six_times_an_hour() {
    let (_tmp, store) = open();
    assert_eq!(MAX_LIVE_LINKS_PER_PROJECT, 8);
    assert_eq!(MAX_AGENT_REQUESTS_PER_HOUR, 6);

    // Eight live links is the ceiling of a project, whoever filed them.
    let hub = project(&store, "hub").await;
    for n in 0..MAX_LIVE_LINKS_PER_PROJECT {
        let spoke = project(&store, &format!("spoke-{n}")).await;
        created(
            store
                .writer
                .link_request(human_request(hub, spoke, false))
                .await
                .unwrap(),
        );
    }
    let extra = project(&store, "one-too-many").await;
    assert_eq!(
        store
            .writer
            .link_request(human_request(hub, extra, false))
            .await
            .unwrap(),
        LinkRequestOutcome::Refused(RequestRefusal::TooManyLive)
    );
    assert_eq!(
        store
            .writer
            .link_request(human_request(extra, hub, false))
            .await
            .unwrap(),
        LinkRequestOutcome::Refused(RequestRefusal::TooManyLive),
        "the destination's ceiling counts too"
    );

    // Six requests an hour from the agents of one project, for six different
    // destinations; the seventh waits for the hour to pass.
    let busy = project(&store, "busy").await;
    for n in 0..MAX_AGENT_REQUESTS_PER_HOUR {
        let target = project(&store, &format!("target-{n}")).await;
        created(
            store
                .writer
                .link_request(agent_request(busy, CLAUDE, "s-busy", target))
                .await
                .unwrap(),
        );
    }
    let seventh = project(&store, "target-6").await;
    assert_eq!(
        store
            .writer
            .link_request(agent_request(busy, CLAUDE, "s-busy", seventh))
            .await
            .unwrap(),
        LinkRequestOutcome::Refused(RequestRefusal::TooManyRecent)
    );
    // The root user is not rate limited by the agents' budget.
    created(
        store
            .writer
            .link_request(human_request(busy, seventh, false))
            .await
            .unwrap(),
    );
    // An hour later the agents may ask again: move the requests' stored time.
    raw(&store)
        .execute(
            "UPDATE agent_links SET requested_at = requested_at - ?1 WHERE requested_by = 'agent'",
            [HOUR_US + 1],
        )
        .unwrap();
    let next = project(&store, "target-7").await;
    created(
        store
            .writer
            .link_request(agent_request(busy, CLAUDE, "s-busy", next))
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn agent_bus_regression_the_chat_shows_cross_project_text_only_for_mail_sent_during_the_link()
{
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    send(&store, b, CODEX, a, CLAUDE, "BEFORE-TEXT").await;
    tick().await;
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    tick().await;
    let during = send(&store, b, CODEX, a, CLAUDE, "DURING-TEXT").await;
    tick().await;
    store
        .writer
        .link_close(id, LinkCloser::Human)
        .await
        .unwrap();
    tick().await;
    send(&store, b, CODEX, a, CLAUDE, "AFTER-TEXT").await;

    let items = store.reader.chat_timeline(key(a), 100).await.unwrap();
    let mails: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            ChatItem::Mail(mail) => Some(mail),
            ChatItem::Post(_) => None,
        })
        .collect();
    assert_eq!(mails.len(), 3);
    for mail in mails {
        if mail.id == during {
            assert_eq!(mail.scope, ChatScope::Linked);
            assert_eq!(mail.body.as_deref(), Some("DURING-TEXT"));
        } else {
            assert_eq!(mail.scope, ChatScope::Hidden, "{mail:?}");
            assert!(mail.body.is_none() && mail.subject.is_none(), "{mail:?}");
            assert_eq!(mail.from.project, "web", "who and where stay visible");
        }
    }
}

#[tokio::test]
async fn agent_bus_regression_every_step_of_a_link_is_in_the_trail() {
    let (_tmp, store) = open();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, false))
            .await
            .unwrap(),
    );
    store.writer.link_confirm_web(id, key(a)).await.unwrap(); // refused
    store.writer.link_confirm_web(id, key(b)).await.unwrap();
    store
        .writer
        .link_close(id, LinkCloser::Human)
        .await
        .unwrap();

    let events = store.reader.agent_link_events(id, 50).await.unwrap();
    let names: Vec<&str> = events
        .iter()
        .map(|(_, event, _, _)| event.as_str())
        .collect();
    for wanted in ["requested", "confirm_refused", "confirmed", "closed"] {
        assert!(names.contains(&wanted), "{wanted} missing in {names:?}");
    }
    for op in [
        "agent_link_requested",
        "agent_link_confirmed",
        "agent_link_closed",
    ] {
        assert_eq!(audit_count(&store, op).await, 1, "{op}");
    }
    // An expiry is audited too, once, when the sweep ends the link.
    let c = project(&store, "third").await;
    let live = created(
        store
            .writer
            .link_request(human_request(a, c, true))
            .await
            .unwrap(),
    );
    let ends = store
        .reader
        .agent_link(live)
        .await
        .unwrap()
        .unwrap()
        .expires_at;
    store
        .writer
        .sweep_message_deadlines(ends, 10)
        .await
        .unwrap();
    store
        .writer
        .sweep_message_deadlines(ends + 1, 10)
        .await
        .unwrap();
    assert_eq!(audit_count(&store, "agent_link_expired").await, 1);
}

#[tokio::test]
async fn agent_bus_regression_a_link_survives_a_restart_with_its_clock() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = ai_memory_store::Store::open(tmp.path()).unwrap();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    send(&store, a, CLAUDE, b, CODEX, "hello").await;
    send(&store, b, CODEX, a, CLAUDE, "hello back").await;
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    let ends = store
        .reader
        .agent_link(id)
        .await
        .unwrap()
        .unwrap()
        .expires_at;
    store.close().await;
    drop(store);

    store = ai_memory_store::Store::open(tmp.path()).unwrap();
    let view = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!(view.state, LinkState::Active);
    assert_eq!(view.expires_at, ends, "a restart does not extend the clock");
    assert!(linked(&store, b, CODEX, a).await);
    assert!(!linked_at(&store, b, CODEX, a, ends).await);
}
