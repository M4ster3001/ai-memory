//! The loop guard of the agent bus (V107): per-pair breakers instead of an
//! hourly wake budget.
//!
//! Pinned here: a pair that ping-pongs, repeats one body or ends a deep chain
//! of mail-woken turns is paused, and only that pair; the mail is still
//! delivered; a human prompt in a session of either side resumes it, but a
//! prompt that is only the wake text does not; and the dashboard resume
//! works and is audited.

use ai_memory_core::message_wake::wake_text;
use ai_memory_core::{
    AgentKind, MessageClaim, MessageId, MessageNature, MessagePriority, NewAgentMessage,
    NewObservation, NewSession, NotifyOn, ObservationKind, OwnerFilter, ProjectId, Sanitized,
    Sanitizer, SessionId, WorkspaceId,
};
use ai_memory_store::{AgentSide, MAX_THREAD_DEPTH, PING_PONG_TURNS, PauseReason, Store};

type Scope = (WorkspaceId, ProjectId);

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

async fn send(store: &Store, at: Scope, from: AgentKind, to: AgentKind, body: &str) -> MessageId {
    store
        .writer
        .insert_message(NewAgentMessage {
            from_workspace_id: at.0,
            from_project_id: at.1,
            from_agent: from,
            from_session_id: None,
            from_owner_user: None,
            to_workspace_id: at.0,
            to_project_id: at.1,
            to_agent: Some(to),
            subject: None,
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

async fn pop(store: &Store, at: Scope, agent: AgentKind, id: MessageId) {
    let popped = store
        .writer
        .pop_message(
            MessageClaim {
                workspace_id: at.0,
                project_id: at.1,
                claiming_agent: agent,
                claiming_session: None,
                claiming_user: None,
            },
            Some(id),
        )
        .await
        .unwrap();
    assert!(popped.is_some());
}

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

/// Whether `to` would not be woken for the pending mail `from` sent it.
async fn paused(store: &Store, at: Scope, from: AgentKind, to: AgentKind) -> bool {
    let candidates = store
        .reader
        .wake_candidates(at.0, at.1, to, false, 256)
        .await
        .unwrap();
    let from_sender: Vec<bool> = candidates
        .iter()
        .filter(|candidate| candidate.from_agent == from)
        .map(|candidate| candidate.pair_paused)
        .collect();
    assert!(
        !from_sender.is_empty(),
        "{from:?} has mail pending for {to:?}"
    );
    from_sender.iter().all(|paused| *paused)
}

fn side(at: Scope, agent: AgentKind) -> AgentSide {
    AgentSide {
        workspace_id: at.0,
        project_id: at.1,
        agent,
    }
}

#[tokio::test]
async fn a_pair_that_ping_pongs_is_paused_alone_and_a_human_prompt_resumes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let at = project(&store, "api").await;
    let (claude, codex, gemini) = (
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::GeminiCli,
    );

    for turn in 0..PING_PONG_TURNS {
        let (from, to) = if turn % 2 == 0 {
            (claude, codex)
        } else {
            (codex, claude)
        };
        send(
            &store,
            at,
            from,
            to,
            &format!("plan step {}", "y".repeat(turn + 1)),
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
        "{} messages make {} direction changes, one short",
        PING_PONG_TURNS,
        PING_PONG_TURNS - 1
    );
    send(&store, at, claude, codex, "one more reply").await;
    let pauses = store.reader.paused_agent_pairs(10).await.unwrap();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(pauses[0].reason, PauseReason::PingPong);
    assert!(paused(&store, at, codex, claude).await);
    assert!(paused(&store, at, claude, codex).await);

    // A third agent still wakes either side.
    send(&store, at, gemini, codex, "an unrelated question").await;
    assert!(!paused(&store, at, gemini, codex).await);

    // The wake text a resumed session receives as its prompt is no human.
    let text = wake_text(claude, "default", "api", 1, MessageId::new());
    prompt(&store, at, claude, "s-claude", &text).await;
    assert!(paused(&store, at, codex, claude).await);

    prompt(&store, at, claude, "s-claude", "keep going with the review").await;
    assert!(!paused(&store, at, codex, claude).await);
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
    // Counting restarts at the prompt: one more message does not trip again.
    send(&store, at, claude, codex, "next step").await;
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
async fn the_same_body_three_times_pauses_the_pair_and_the_dashboard_resumes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let at = project(&store, "web").await;
    let (claude, codex) = (AgentKind::ClaudeCode, AgentKind::Codex);
    send(
        &store,
        at,
        claude,
        codex,
        "Build failed: retry 1 of job 0190f0a4",
    )
    .await;
    send(
        &store,
        at,
        claude,
        codex,
        "build failed:   retry 2 of job 7f00aa11",
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
    send(
        &store,
        at,
        claude,
        codex,
        "BUILD FAILED: retry 3 of job 12345678",
    )
    .await;
    let pauses = store.reader.paused_agent_pairs(10).await.unwrap();
    assert_eq!(pauses.len(), 1);
    assert_eq!(pauses[0].reason, PauseReason::RepeatedContent);
    assert_eq!(pauses[0].trip_count, 1);
    assert!(paused(&store, at, claude, codex).await);

    assert!(
        store
            .writer
            .resume_agent_pair(side(at, codex), side(at, claude))
            .await
            .unwrap(),
        "either order names the pair"
    );
    assert!(!paused(&store, at, claude, codex).await);
    assert!(
        !store
            .writer
            .resume_agent_pair(side(at, codex), side(at, claude))
            .await
            .unwrap()
    );
    // The copies before the resume no longer count.
    send(&store, at, claude, codex, "build failed: retry 4 of job 1").await;
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
async fn a_chain_of_three_agents_trips_the_thread_depth() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let at = project(&store, "ring").await;
    let ring = [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::GeminiCli,
    ];
    let depth = usize::try_from(MAX_THREAD_DEPTH).unwrap();
    for hop in 0..=depth {
        let (from, to) = (ring[hop % 3], ring[(hop + 1) % 3]);
        let id = send(
            &store,
            at,
            from,
            to,
            &format!("ring message {}", "x".repeat(hop)),
        )
        .await;
        assert!(
            store
                .reader
                .paused_agent_pairs(10)
                .await
                .unwrap()
                .is_empty(),
            "hop {hop} is within the depth"
        );
        pop(&store, at, to, id).await;
    }
    let (from, to) = (ring[(depth + 1) % 3], ring[(depth + 2) % 3]);
    send(&store, at, from, to, "one hop too deep").await;
    let pauses = store.reader.paused_agent_pairs(10).await.unwrap();
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    assert_eq!(pauses[0].reason, PauseReason::ThreadDepth);
    assert!(paused(&store, at, from, to).await);

    // A human prompt to the sender starts a new chain at hop 0.
    prompt(&store, at, from, "s-ring", "start over").await;
    pop_all(&store, at, to).await;
    send(&store, at, from, to, "a fresh start").await;
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn pop_all(store: &Store, at: Scope, agent: AgentKind) {
    let pending = store
        .reader
        .wake_candidates(at.0, at.1, agent, false, 256)
        .await
        .unwrap();
    for candidate in pending {
        pop(store, at, agent, candidate.id).await;
    }
}
