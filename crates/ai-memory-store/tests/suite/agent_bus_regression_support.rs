//! Helpers shared by the agent bus regression suite of this crate
//! (`docs/agent-bus/TESTING.md`). Nothing here is a test.

use std::time::Duration;

use ai_memory_core::message_wake::WakeHandleKind;
use ai_memory_core::{
    AgentKind, AgentMessage, MessageBox, MessageClaim, MessageId, MessageNature, MessagePriority,
    NewAgentMessage, NewObservation, NewSession, NotifyOn, ObservationKind, OwnerFilter, ProjectId,
    Sanitized, Sanitizer, SessionId, WorkspaceId,
};
use ai_memory_store::{
    AgentSide, LinkRequester, MessageListFilter, NewLinkRequest, ProjectKey, Store, WakeCandidate,
};

/// A project, by ids.
pub type Scope = (WorkspaceId, ProjectId);

pub const HOUR_US: i64 = 3_600 * 1_000_000;
pub const MINUTE_US: i64 = 60 * 1_000_000;

/// Open a store in a fresh directory. The directory lives as long as the
/// returned guard.
pub fn open() -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    (tmp, store)
}

pub async fn project(store: &Store, name: &str) -> Scope {
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

pub fn key(at: Scope) -> ProjectKey {
    ProjectKey {
        workspace_id: at.0,
        project_id: at.1,
    }
}

pub fn side(at: Scope, agent: AgentKind) -> AgentSide {
    AgentSide {
        workspace_id: at.0,
        project_id: at.1,
        agent,
    }
}

pub fn now_us() -> i64 {
    jiff::Timestamp::now().as_microsecond()
}

/// Let the clock move at least one tick, so two writes never share a
/// microsecond when a test orders them.
pub async fn tick() {
    tokio::time::sleep(Duration::from_millis(3)).await;
}

/// A message with every field a test does not care about defaulted.
pub fn new_message(
    from: Scope,
    from_agent: AgentKind,
    to: Scope,
    to_agent: Option<AgentKind>,
    body: &str,
) -> NewAgentMessage {
    NewAgentMessage {
        from_workspace_id: from.0,
        from_project_id: from.1,
        from_agent,
        from_session_id: None,
        from_owner_user: None,
        to_workspace_id: to.0,
        to_project_id: to.1,
        to_agent,
        subject: Some("a subject".into()),
        body: body.into(),
        nature: MessageNature::Unknown,
        evidence: None,
        priority: MessagePriority::Normal,
        notify: NotifyOn::DEFAULT,
        ttl_secs: None,
    }
}

pub async fn send(
    store: &Store,
    from: Scope,
    from_agent: AgentKind,
    to: Scope,
    to_agent: AgentKind,
    body: &str,
) -> MessageId {
    store
        .writer
        .insert_message(new_message(from, from_agent, to, Some(to_agent), body))
        .await
        .unwrap()
}

pub fn claim(at: Scope, agent: AgentKind) -> MessageClaim {
    MessageClaim {
        workspace_id: at.0,
        project_id: at.1,
        claiming_agent: agent,
        claiming_session: None,
        claiming_user: None,
    }
}

/// Pop `id` from `at`'s inbox as `agent`.
pub async fn pop(
    store: &Store,
    at: Scope,
    agent: AgentKind,
    id: MessageId,
) -> Option<AgentMessage> {
    store
        .writer
        .pop_message(claim(at, agent), Some(id))
        .await
        .unwrap()
}

pub async fn inbox(store: &Store, at: Scope) -> Vec<AgentMessage> {
    store
        .reader
        .list_messages(at.0, at.1, MessageBox::Inbox, 200)
        .await
        .unwrap()
}

/// The sender's own view of one message in every state.
pub async fn outbox_row(store: &Store, from: Scope, id: MessageId) -> AgentMessage {
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

/// Start a session of `agent` in `at` (which registers the harness there).
pub async fn start_session(store: &Store, at: Scope, agent: AgentKind) -> SessionId {
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

async fn hook_event(
    store: &Store,
    at: Scope,
    agent: AgentKind,
    session: &str,
    kind: ObservationKind,
    text: &str,
    handle: Option<WakeHandleKind>,
) {
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
                    body: text.into(),
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
        .unwrap();
}

/// A user prompt in a hook session (which creates the session).
pub async fn prompt(store: &Store, at: Scope, agent: AgentKind, session: &str, text: &str) {
    hook_event(
        store,
        at,
        agent,
        session,
        ObservationKind::UserPrompt,
        text,
        None,
    )
    .await;
}

/// A session start that records a wake handle (the session id must be a
/// canonical UUID).
pub async fn session_with_handle(
    store: &Store,
    at: Scope,
    agent: AgentKind,
    session: &str,
    kind: WakeHandleKind,
) {
    hook_event(
        store,
        at,
        agent,
        session,
        ObservationKind::SessionStart,
        "start",
        Some(kind),
    )
    .await;
}

/// What the wake gate reads for `to`'s pending mail now.
pub async fn candidates(store: &Store, to: Scope, agent: AgentKind) -> Vec<WakeCandidate> {
    store
        .reader
        .wake_candidates(to.0, to.1, agent, false, 256)
        .await
        .unwrap()
}

/// The same read at `at_us`, to look past a link's clock.
pub async fn candidates_at(
    store: &Store,
    to: Scope,
    agent: AgentKind,
    at_us: i64,
) -> Vec<WakeCandidate> {
    store
        .reader
        .wake_candidates_at(to.0, to.1, agent, false, 256, at_us)
        .await
        .unwrap()
}

/// Whether every candidate of `to` sent from `from` carries the `linked`
/// label at `at_us`. Panics when there is none, so a test never passes on
/// an empty read.
pub async fn linked_at(
    store: &Store,
    to: Scope,
    agent: AgentKind,
    from: Scope,
    at_us: i64,
) -> bool {
    let labels: Vec<bool> = candidates_at(store, to, agent, at_us)
        .await
        .iter()
        .filter(|candidate| candidate.from_project_id == from.1)
        .map(|candidate| candidate.linked)
        .collect();
    assert!(!labels.is_empty(), "mail from that project is pending");
    labels.iter().all(|linked| *linked)
}

pub async fn linked(store: &Store, to: Scope, agent: AgentKind, from: Scope) -> bool {
    linked_at(store, to, agent, from, now_us()).await
}

/// Whether every pending message `from` sent `to` is held back by a paused
/// pair.
pub async fn paused(store: &Store, at: Scope, from: AgentKind, to: AgentKind) -> bool {
    let from_sender: Vec<bool> = candidates(store, at, to)
        .await
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

pub fn human_request(from: Scope, to: Scope, confirm_now: bool) -> NewLinkRequest {
    NewLinkRequest {
        from: key(from),
        to: key(to),
        reason: "share the API shape".into(),
        valid_secs: 7_200,
        by: LinkRequester::Human,
        confirm_now,
    }
}

pub fn agent_request(from: Scope, agent: AgentKind, session: &str, to: Scope) -> NewLinkRequest {
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

/// The id of a request that must have created a link.
pub fn created(outcome: ai_memory_store::LinkRequestOutcome) -> ai_memory_core::AgentLinkId {
    match outcome {
        ai_memory_store::LinkRequestOutcome::Created(id, _) => id,
        other => panic!("expected a new link, got {other:?}"),
    }
}

/// A second, raw connection to the store's file, for the one thing the API
/// must not offer: moving a stored time.
pub fn raw(store: &Store) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn
}

/// The stored `hop` of a message.
pub fn hop(store: &Store, id: MessageId) -> i64 {
    raw(store)
        .query_row(
            "SELECT hop FROM agent_messages WHERE id = ?1",
            [&id.as_bytes()[..]],
            |row| row.get(0),
        )
        .unwrap()
}

/// How many audit rows of `op` exist.
pub async fn audit_count(store: &Store, op: &str) -> usize {
    store
        .reader
        .list_audit_events(ai_memory_store::AuditLogFilter {
            op: Some(op.to_owned()),
            limit: 200,
            ..ai_memory_store::AuditLogFilter::default()
        })
        .await
        .unwrap()
        .len()
}
