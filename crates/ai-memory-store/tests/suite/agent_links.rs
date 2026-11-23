//! Links between two projects (V109).
//!
//! Pinned here: a request never wakes anyone; only the destination confirms
//! (the web page, or an agent whose session was shown the request and then
//! saw a user prompt); the requester never confirms its own request; a link
//! wakes exactly its two projects and only until its clock runs out, even
//! before the sweep ends it; one live link exists per pair; either side can
//! close; a purged project ends its links in the audit log; the chat read
//! shows another project's text only for mail sent while a link was active;
//! and a human post from the dashboard resumes the project's paused pairs.

use std::time::Duration;

use ai_memory_core::agent_links::{ConfirmRefusal, LinkState};
use ai_memory_core::message_wake::{WakeOutcome, wake_text};
use ai_memory_core::{
    AgentKind, DeliveryReason, DeliveryState, MessageId, MessageNature, MessagePriority,
    NewAgentMessage, NewObservation, NewSession, NotifyOn, ObservationKind, OwnerFilter, ProjectId,
    Sanitized, Sanitizer, SessionId, WorkspaceId,
};
use ai_memory_store::{
    AgentConfirm, ChatItem, ChatScope, LinkCloseOutcome, LinkCloser, LinkConfirmOutcome,
    LinkRequestOutcome, LinkRequester, NewLinkRequest, NewRoomPost, ProjectKey, RequestRefusal,
    Store,
};

type Scope = (WorkspaceId, ProjectId);

const HOUR_US: i64 = 3_600 * 1_000_000;

async fn project(store: &Store, name: &str) -> Scope {
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

fn key(at: Scope) -> ProjectKey {
    ProjectKey {
        workspace_id: at.0,
        project_id: at.1,
    }
}

fn now_us() -> i64 {
    jiff::Timestamp::now().as_microsecond()
}

async fn pause() {
    tokio::time::sleep(Duration::from_millis(3)).await;
}

async fn send(
    store: &Store,
    from: Scope,
    from_agent: AgentKind,
    to: Scope,
    to_agent: AgentKind,
    body: &str,
) -> MessageId {
    store
        .writer
        .insert_message(NewAgentMessage {
            from_workspace_id: from.0,
            from_project_id: from.1,
            from_agent,
            from_session_id: None,
            from_owner_user: None,
            to_workspace_id: to.0,
            to_project_id: to.1,
            to_agent: Some(to_agent),
            subject: Some("subject".into()),
            body: body.into(),
            nature: MessageNature::Unknown,
            evidence: None,
            priority: MessagePriority::Normal,
            notify: NotifyOn::DEFAULT,
            ttl_secs: None,
        })
        .await
        .unwrap()
}

/// A user prompt in a hook session (which creates the session).
async fn prompt(store: &Store, at: Scope, agent: AgentKind, session: &str, text: &str) {
    let id = SessionId::from_native(session);
    store
        .writer
        .admit_reported_hook_session_event(
            NewSession {
                id,
                workspace_id: at.0,
                project_id: at.1,
                agent_kind: agent,
                cwd: None,
                actor_user: None,
                occurred_at: None,
            },
            Sanitized::new(
                NewObservation {
                    occurred_at: None,
                    session_id: id,
                    workspace_id: at.0,
                    project_id: at.1,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: text.into(),
                    importance: 1,
                    file_path: None,
                },
                &Sanitizer::builtin(),
            ),
            OwnerFilter::Any,
            None,
            None,
            None,
        )
        .await
        .unwrap();
}

fn human_request(from: Scope, to: Scope, confirm_now: bool) -> NewLinkRequest {
    NewLinkRequest {
        from: key(from),
        to: key(to),
        reason: "share the API shape".into(),
        valid_secs: 7_200,
        by: LinkRequester::Human,
        confirm_now,
    }
}

fn agent_request(from: Scope, agent: AgentKind, session: &str, to: Scope) -> NewLinkRequest {
    NewLinkRequest {
        from: key(from),
        to: key(to),
        reason: "need the schema".into(),
        valid_secs: 7_200,
        by: LinkRequester::Agent {
            agent,
            session: Some(SessionId::from_native(session)),
        },
        confirm_now: false,
    }
}

async fn linked_label(store: &Store, at: Scope, agent: AgentKind, from: Scope, at_us: i64) -> bool {
    let candidates = store
        .reader
        .wake_candidates_at(at.0, at.1, agent, false, 256, at_us)
        .await
        .unwrap();
    let labels: Vec<bool> = candidates
        .iter()
        .filter(|candidate| candidate.from_project_id == from.1)
        .map(|candidate| candidate.linked)
        .collect();
    assert!(!labels.is_empty(), "mail from that project is pending");
    labels.iter().all(|linked| *linked)
}

fn created(outcome: LinkRequestOutcome) -> ai_memory_core::AgentLinkId {
    match outcome {
        LinkRequestOutcome::Created(id, _) => id,
        other => panic!("expected a new link, got {other:?}"),
    }
}

#[tokio::test]
async fn a_request_wakes_nobody_and_the_destination_page_opens_exactly_the_pair() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b, c) = (
        project(&store, "api").await,
        project(&store, "web").await,
        project(&store, "other").await,
    );
    send(
        &store,
        a,
        AgentKind::ClaudeCode,
        b,
        AgentKind::Codex,
        "from a",
    )
    .await;
    send(
        &store,
        c,
        AgentKind::ClaudeCode,
        b,
        AgentKind::Codex,
        "from c",
    )
    .await;
    send(
        &store,
        b,
        AgentKind::Codex,
        a,
        AgentKind::ClaudeCode,
        "from b",
    )
    .await;
    assert!(!linked_label(&store, b, AgentKind::Codex, a, now_us()).await);

    let id = created(
        store
            .writer
            .link_request(human_request(a, b, false))
            .await
            .unwrap(),
    );
    assert!(
        !linked_label(&store, b, AgentKind::Codex, a, now_us()).await,
        "a request never wakes anyone"
    );

    // The requester's own page is not the destination's.
    assert_eq!(
        store.writer.link_confirm_web(id, key(a)).await.unwrap(),
        LinkConfirmOutcome::Refused(ConfirmRefusal::NotDestination)
    );
    assert!(!linked_label(&store, b, AgentKind::Codex, a, now_us()).await);

    assert!(matches!(
        store.writer.link_confirm_web(id, key(b)).await.unwrap(),
        LinkConfirmOutcome::Confirmed { .. }
    ));
    assert!(linked_label(&store, b, AgentKind::Codex, a, now_us()).await);
    assert!(
        linked_label(&store, a, AgentKind::ClaudeCode, b, now_us()).await,
        "mail wakes in both directions"
    );
    assert!(
        !linked_label(&store, b, AgentKind::Codex, c, now_us()).await,
        "a third project stays closed"
    );

    // Confirming twice changes nothing.
    assert_eq!(
        store.writer.link_confirm_web(id, key(b)).await.unwrap(),
        LinkConfirmOutcome::Refused(ConfirmRefusal::NotRequested)
    );
}

#[tokio::test]
async fn an_expired_link_wakes_nobody_even_before_the_sweep_ends_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    send(
        &store,
        a,
        AgentKind::ClaudeCode,
        b,
        AgentKind::Codex,
        "hello",
    )
    .await;
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    assert!(linked_label(&store, b, AgentKind::Codex, a, now_us()).await);

    let later = now_us() + 2 * HOUR_US + 1;
    assert!(
        !linked_label(&store, b, AgentKind::Codex, a, later).await,
        "past its clock the link wakes nobody, swept or not"
    );
    let view = store
        .reader
        .agent_links_at(None, true, 10, later)
        .await
        .unwrap();
    assert_eq!(view[0].state, LinkState::Expired, "read as expired at once");

    // The sweep then ends it for real, in the trail.
    store
        .writer
        .sweep_message_deadlines(later, 10)
        .await
        .unwrap();
    let stored = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!(stored.close_reason.as_deref(), Some("expired"));
    let events = store.reader.agent_link_events(id, 20).await.unwrap();
    assert!(events.iter().any(|(_, event, _, _)| event == "expired"));
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
async fn one_live_link_per_pair_and_a_closed_pair_can_ask_again() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, false))
            .await
            .unwrap(),
    );
    assert_eq!(
        store
            .writer
            .link_request(human_request(b, a, true))
            .await
            .unwrap(),
        LinkRequestOutcome::Existing(id, LinkState::Requested),
        "the other direction finds the same link and cannot skip the confirmation"
    );
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
            .writer
            .link_close(id, LinkCloser::Human)
            .await
            .unwrap(),
        LinkCloseOutcome::Closed(LinkState::Requested)
    );
    let again = created(
        store
            .writer
            .link_request(human_request(b, a, false))
            .await
            .unwrap(),
    );
    assert_ne!(again, id);
}

#[tokio::test]
async fn an_agent_confirms_only_after_its_user_answered_the_request_it_was_shown() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    prompt(
        &store,
        a,
        AgentKind::ClaudeCode,
        "s-a",
        "connect with the web project",
    )
    .await;
    prompt(&store, b, AgentKind::Codex, "s-b", "working on the form").await;
    let id = created(
        store
            .writer
            .link_request(agent_request(a, AgentKind::ClaudeCode, "s-a", b))
            .await
            .unwrap(),
    );
    let confirm = |project: Scope, agent: AgentKind, session: &str| AgentConfirm {
        link: id,
        project: key(project),
        agent,
        session: Some(SessionId::from_native(session)),
    };
    let refused = |outcome: LinkConfirmOutcome| match outcome {
        LinkConfirmOutcome::Refused(refusal) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    };

    // The requester never confirms its own request.
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(confirm(a, AgentKind::ClaudeCode, "s-a"))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NotDestination
    );
    // The destination must have been shown the request first.
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(confirm(b, AgentKind::Codex, "s-b"))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NotPresented
    );
    // A session of the destination the server never recorded is no agent.
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(AgentConfirm {
                    session: None,
                    ..confirm(b, AgentKind::Codex, "s-b")
                })
                .await
                .unwrap()
        ),
        ConfirmRefusal::NoSession
    );
    // The requester cannot present itself the request.
    assert!(
        store
            .writer
            .link_present(key(a), AgentKind::ClaudeCode, SessionId::from_native("s-a"))
            .await
            .unwrap()
            .is_empty()
    );

    // A "yes" typed before the request was shown does not count.
    prompt(&store, b, AgentKind::Codex, "s-b", "yes, go ahead").await;
    pause().await;
    let shown = store
        .writer
        .link_present(key(b), AgentKind::Codex, SessionId::from_native("s-b"))
        .await
        .unwrap();
    assert_eq!(shown, vec![id]);
    assert!(
        store
            .writer
            .link_present(key(b), AgentKind::Codex, SessionId::from_native("s-b"))
            .await
            .unwrap()
            .is_empty(),
        "showing it again does not move the moment it was shown"
    );
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(confirm(b, AgentKind::Codex, "s-b"))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NoUserAnswer,
        "a prompt older than the presentation is no answer"
    );

    // Neither is the wake text the bus sends.
    pause().await;
    let text = wake_text(AgentKind::Codex, "default", "web", 1, MessageId::new());
    prompt(&store, b, AgentKind::Codex, "s-b", &text).await;
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(confirm(b, AgentKind::Codex, "s-b"))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NoUserAnswer
    );

    // Another session of the destination was not the one shown.
    prompt(
        &store,
        b,
        AgentKind::ClaudeCode,
        "s-b2",
        "yes, connect them",
    )
    .await;
    assert_eq!(
        refused(
            store
                .writer
                .link_confirm_agent(confirm(b, AgentKind::ClaudeCode, "s-b2"))
                .await
                .unwrap()
        ),
        ConfirmRefusal::NotPresented
    );

    pause().await;
    prompt(&store, b, AgentKind::Codex, "s-b", "yes, connect them").await;
    assert!(matches!(
        store
            .writer
            .link_confirm_agent(confirm(b, AgentKind::Codex, "s-b"))
            .await
            .unwrap(),
        LinkConfirmOutcome::Confirmed { .. }
    ));
    let view = store.reader.agent_link(id).await.unwrap().unwrap();
    assert_eq!(view.state, LinkState::Active);
    assert_eq!(view.confirmed_via.as_deref(), Some("agent"));

    // Every refusal is in the trail, as a code and never as text.
    let events = store.reader.agent_link_events(id, 50).await.unwrap();
    let refusals: Vec<&str> = events
        .iter()
        .filter(|(_, event, _, _)| event == "confirm_refused")
        .map(|(_, _, _, detail)| detail.as_str())
        .collect();
    assert!(refusals.contains(&"not_destination"), "{refusals:?}");
    assert!(refusals.contains(&"no_user_answer"), "{refusals:?}");
}

#[tokio::test]
async fn either_side_may_close_and_a_third_project_may_not() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b, c) = (
        project(&store, "api").await,
        project(&store, "web").await,
        project(&store, "other").await,
    );
    send(&store, a, AgentKind::ClaudeCode, b, AgentKind::Codex, "hi").await;
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
                    agent: AgentKind::Codex
                }
            )
            .await
            .unwrap(),
        LinkCloseOutcome::NotAllowed
    );
    assert!(linked_label(&store, b, AgentKind::Codex, a, now_us()).await);
    assert_eq!(
        store
            .writer
            .link_close(
                id,
                LinkCloser::Agent {
                    project: key(b),
                    agent: AgentKind::Codex
                }
            )
            .await
            .unwrap(),
        LinkCloseOutcome::Closed(LinkState::Active)
    );
    assert!(!linked_label(&store, b, AgentKind::Codex, a, now_us()).await);
    assert_eq!(
        store.reader.agent_link(id).await.unwrap().unwrap().state,
        LinkState::Closed
    );
}

#[tokio::test]
async fn a_purged_project_ends_its_links_in_the_audit_log_without_text() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
async fn the_chat_shows_another_projects_text_only_for_mail_sent_during_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    send(
        &store,
        a,
        AgentKind::ClaudeCode,
        a,
        AgentKind::Codex,
        "INSIDE-TEXT",
    )
    .await;
    pause().await;
    send(
        &store,
        b,
        AgentKind::Codex,
        a,
        AgentKind::ClaudeCode,
        "BEFORE-TEXT",
    )
    .await;
    pause().await;
    let id = created(
        store
            .writer
            .link_request(human_request(a, b, true))
            .await
            .unwrap(),
    );
    pause().await;
    let during = send(
        &store,
        b,
        AgentKind::Codex,
        a,
        AgentKind::ClaudeCode,
        "DURING-TEXT",
    )
    .await;
    pause().await;
    store
        .writer
        .link_close(id, LinkCloser::Human)
        .await
        .unwrap();
    pause().await;
    send(
        &store,
        b,
        AgentKind::Codex,
        a,
        AgentKind::ClaudeCode,
        "AFTER-TEXT",
    )
    .await;

    let items = store.reader.chat_timeline(key(a), 100).await.unwrap();
    let mails: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            ChatItem::Mail(mail) => Some(mail),
            ChatItem::Post(_) => None,
        })
        .collect();
    assert_eq!(mails.len(), 4);
    let by_scope = |scope: ChatScope| mails.iter().filter(|m| m.scope == scope).count();
    assert_eq!(by_scope(ChatScope::Internal), 1);
    assert_eq!(by_scope(ChatScope::Linked), 1);
    assert_eq!(by_scope(ChatScope::Hidden), 2);
    for mail in &mails {
        match mail.scope {
            ChatScope::Hidden => {
                assert!(mail.body.is_none() && mail.subject.is_none(), "{mail:?}");
                assert_eq!(mail.from.project, "web", "who and where stay visible");
            }
            ChatScope::Linked => {
                assert_eq!(mail.id, during);
                assert_eq!(mail.body.as_deref(), Some("DURING-TEXT"));
            }
            ChatScope::Internal => assert_eq!(mail.body.as_deref(), Some("INSIDE-TEXT")),
        }
    }
    // The other project's own page applies the same rule to its side.
    let theirs = store.reader.chat_timeline(key(b), 100).await.unwrap();
    let texts: Vec<String> = theirs
        .iter()
        .filter_map(|item| match item {
            ChatItem::Mail(mail) => mail.body.clone(),
            ChatItem::Post(_) => None,
        })
        .collect();
    assert_eq!(texts, vec!["DURING-TEXT".to_owned()]);
}

#[tokio::test]
async fn the_chat_folds_receipts_under_their_mail_and_follows_the_delivery_state() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "api").await;
    let id = send(
        &store,
        a,
        AgentKind::ClaudeCode,
        a,
        AgentKind::Codex,
        "please look",
    )
    .await;
    store
        .writer
        .record_wake_report(
            a.0,
            a.1,
            AgentKind::Codex,
            vec![id],
            WakeOutcome::from_report(
                DeliveryState::WakeFailed,
                Some(DeliveryReason::SessionBusy),
                None,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let items = store.reader.chat_timeline(key(a), 100).await.unwrap();
    let mails: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            ChatItem::Mail(mail) => Some(mail),
            ChatItem::Post(_) => None,
        })
        .collect();
    assert_eq!(mails.len(), 1, "a receipt is folded, never its own bubble");
    assert_eq!(mails[0].delivery_state, "wake_failed");
    assert_eq!(mails[0].delivery_reason.as_deref(), Some("session_busy"));
    assert_eq!(mails[0].receipts.len(), 1, "{:?}", mails[0].receipts);
    assert_eq!(mails[0].receipts[0].outcome, "wake_failed");
}

#[tokio::test]
async fn a_human_post_resumes_the_projects_paused_pairs_and_still_mentions() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "api").await;
    for n in 1..=3 {
        send(
            &store,
            a,
            AgentKind::ClaudeCode,
            a,
            AgentKind::Codex,
            &format!("Build failed: retry {n} of job 1234567{n}"),
        )
        .await;
    }
    assert_eq!(store.reader.paused_agent_pairs(10).await.unwrap().len(), 1);

    let outcome = store
        .writer
        .room_post_human(NewRoomPost {
            workspace_id: a.0,
            project_id: a.1,
            room: ai_memory_core::rooms::RoomName::parse("general").unwrap(),
            from_agent: AgentKind::Other,
            from_session_id: None,
            from_owner_user: None,
            body: "@codex please take over".into(),
        })
        .await
        .unwrap();
    assert_eq!(outcome.mentioned.len(), 1);
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty(),
        "a human post resumes the project's paused pairs"
    );
    // The plain agent post does not.
    for n in 4..=6 {
        send(
            &store,
            a,
            AgentKind::ClaudeCode,
            a,
            AgentKind::Codex,
            &format!("Build failed: retry {n} of job 7654321{n}"),
        )
        .await;
    }
    assert_eq!(store.reader.paused_agent_pairs(10).await.unwrap().len(), 1);
    store
        .writer
        .room_post(NewRoomPost {
            workspace_id: a.0,
            project_id: a.1,
            room: ai_memory_core::rooms::RoomName::parse("general").unwrap(),
            from_agent: AgentKind::Codex,
            from_session_id: None,
            from_owner_user: None,
            body: "still stuck".into(),
        })
        .await
        .unwrap();
    assert_eq!(store.reader.paused_agent_pairs(10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_fingerprint_moves_with_mail_links_and_expiry() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (a, b) = (project(&store, "api").await, project(&store, "web").await);
    let first = store.reader.chat_fingerprint(key(a)).await.unwrap();
    assert_eq!(first, store.reader.chat_fingerprint(key(a)).await.unwrap());
    send(&store, a, AgentKind::ClaudeCode, a, AgentKind::Codex, "x").await;
    let second = store.reader.chat_fingerprint(key(a)).await.unwrap();
    assert_ne!(first, second);
    store
        .writer
        .link_request(human_request(b, a, false))
        .await
        .unwrap();
    let third = store.reader.chat_fingerprint(key(a)).await.unwrap();
    assert_ne!(
        second, third,
        "a request on the destination changes the view"
    );
    let unrelated = project(&store, "unrelated").await;
    send(
        &store,
        unrelated,
        AgentKind::Codex,
        unrelated,
        AgentKind::ClaudeCode,
        "y",
    )
    .await;
    let fourth = store.reader.chat_fingerprint(key(a)).await.unwrap();
    // Mail elsewhere may move the global delivery counter, never the
    // project's own message or link counts.
    assert_eq!(
        third.split('.').skip(1).collect::<Vec<_>>(),
        fourth.split('.').skip(1).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_project_may_hold_only_so_many_live_links() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let hub = project(&store, "hub").await;
    for n in 0..ai_memory_core::agent_links::MAX_LIVE_LINKS_PER_PROJECT {
        let other = project(&store, &format!("spoke-{n}")).await;
        created(
            store
                .writer
                .link_request(human_request(hub, other, false))
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
}
