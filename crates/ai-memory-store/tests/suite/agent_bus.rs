//! Wake handles (V105) and the agent bus read.
//!
//! The guards pinned here: a handle is recorded only in the same transaction
//! as its hook event and only with the kind of the session's own harness (a
//! mismatch rolls the whole event back); an ending event records nothing; the
//! newest open session is the one listed; and shared mail or mail for a
//! harness without a handle never makes a target.

use std::time::Duration;

use ai_memory_core::message_wake::WakeHandleKind;
use ai_memory_core::{
    AgentKind, MessagePriority, NewAgentMessage, NewObservation, NewSession, NotifyOn,
    ObservationKind, OwnerFilter, ProjectId, Sanitized, Sanitizer, SessionId, WorkspaceId,
};
use ai_memory_store::Store;

const THREAD_A: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718291";
const THREAD_B: &str = "0190f0a4-5c1e-7a2b-9c3d-4e5f60718292";

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

async fn event(
    store: &Store,
    at: (WorkspaceId, ProjectId),
    agent: AgentKind,
    session: &str,
    kind: ObservationKind,
    handle: Option<WakeHandleKind>,
) -> ai_memory_store::StoreResult<ai_memory_store::HookSessionAdmission> {
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
                    kind,
                    extension: None,
                    source_event: None,
                    title: "event".into(),
                    body: "event".into(),
                    importance: 1,
                    file_path: None,
                },
                &Sanitizer::builtin(),
            ),
            OwnerFilter::Any,
            None,
            None,
            handle,
        )
        .await
}

fn to(at: (WorkspaceId, ProjectId), agent: Option<AgentKind>) -> NewAgentMessage {
    NewAgentMessage {
        from_workspace_id: at.0,
        from_project_id: at.1,
        from_agent: AgentKind::ClaudeCode,
        from_session_id: None,
        from_owner_user: None,
        to_workspace_id: at.0,
        to_project_id: at.1,
        to_agent: agent,
        subject: None,
        body: "body".into(),
        nature: ai_memory_core::MessageNature::Unknown,
        evidence: None,
        priority: MessagePriority::Normal,
        notify: NotifyOn::DEFAULT,
        ttl_secs: None,
    }
}

#[tokio::test]
async fn a_handle_of_another_harness_rolls_the_whole_event_back() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let at = project(&store, "api").await;
    let refused = event(
        &store,
        at,
        AgentKind::Codex,
        THREAD_A,
        ObservationKind::SessionStart,
        Some(WakeHandleKind::ClaudeSession),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        store
            .reader
            .wake_handles(at.0, at.1)
            .await
            .unwrap()
            .is_empty()
    );
    let sessions = store
        .reader
        .sessions_for_scope(at.0, at.1, OwnerFilter::Any, true, 10, 0)
        .await
        .unwrap();
    assert!(sessions.is_empty(), "the event was rolled back with it");

    event(
        &store,
        at,
        AgentKind::Codex,
        THREAD_A,
        ObservationKind::SessionStart,
        Some(WakeHandleKind::CodexThread),
    )
    .await
    .unwrap();
    let handles = store.reader.wake_handles(at.0, at.1).await.unwrap();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].0, AgentKind::Codex);
    assert_eq!(handles[0].1.handle, THREAD_A);
    assert!(handles[0].1.open);
}

#[tokio::test]
async fn targets_need_addressed_mail_and_a_handle_and_prefer_an_open_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let at = project(&store, "api").await;
    let other = project(&store, "web").await;
    let reported = Some(WakeHandleKind::CodexThread);
    event(
        &store,
        at,
        AgentKind::Codex,
        THREAD_A,
        ObservationKind::SessionStart,
        reported,
    )
    .await
    .unwrap();
    // A session that ends records nothing, even when it reports a handle.
    event(
        &store,
        other,
        AgentKind::Codex,
        THREAD_B,
        ObservationKind::SessionEnd,
        reported,
    )
    .await
    .ok();
    assert!(
        store
            .reader
            .wake_handles(other.0, other.1)
            .await
            .unwrap()
            .is_empty()
    );

    store.writer.insert_message(to(at, None)).await.unwrap();
    store
        .writer
        .insert_message(to(at, Some(AgentKind::Grok)))
        .await
        .unwrap();
    assert!(store.reader.bus_targets(64, 256).await.unwrap().is_empty());

    let first = store
        .writer
        .insert_message(to(at, Some(AgentKind::Codex)))
        .await
        .unwrap();
    let second = store
        .writer
        .insert_message(to(at, Some(AgentKind::Codex)))
        .await
        .unwrap();
    let targets = store.reader.bus_targets(64, 1).await.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].project, "api");
    assert_eq!(targets[0].agent, AgentKind::Codex);
    assert_eq!(targets[0].handle.handle, THREAD_A);
    assert_eq!(targets[0].pending, [first]);
    assert!(targets[0].truncated, "a second id waits beyond the limit");
    let all = store.reader.bus_targets(64, 256).await.unwrap();
    assert_eq!(all[0].pending, [first, second]);
    assert!(!all[0].truncated);

    // A newer session in the same project, then the older one ends: the
    // open one is listed whatever their order.
    tokio::time::sleep(Duration::from_millis(5)).await;
    event(
        &store,
        at,
        AgentKind::Codex,
        THREAD_B,
        ObservationKind::SessionStart,
        reported,
    )
    .await
    .unwrap();
    store
        .writer
        .end_session(SessionId::from_native(THREAD_B), None)
        .await
        .unwrap();
    let targets = store.reader.bus_targets(64, 256).await.unwrap();
    assert_eq!(targets[0].handle.handle, THREAD_A, "the open session wins");
    assert!(targets[0].handle.open);
}
