//! Agent bus regression suite, schema layer: a store written before the bus
//! (V104) is opened by the current code (V109) and keeps every message,
//! receipt and delivery event (`docs/agent-bus/TESTING.md`).
//!
//! The migrations V105 to V109 rebuild `agent_messages` and the delivery
//! timeline to widen their CHECK lists, so a regression here would lose or
//! rewrite mail. The test builds the old schema with the migration runner
//! itself, seeds old-shaped rows through SQL, and then opens the file with
//! `Store::open`, exactly as an upgraded server does.

use ai_memory_core::{
    AgentKind, DeliveryReason, DeliveryState, MessageBox, MessageId, MessageKind, MessageState,
    ProjectId, ReceiptOutcome, WorkspaceId,
};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

use super::agent_bus_regression_support::{Scope, claim, human_request, send};

fn id(n: u8) -> Vec<u8> {
    let mut bytes = vec![0_u8; 16];
    bytes[0] = n;
    bytes[15] = n;
    bytes
}

fn scope(workspace: u8, project: u8) -> Scope {
    (
        WorkspaceId::from_slice(&id(workspace)).unwrap(),
        ProjectId::from_slice(&id(project)).unwrap(),
    )
}

const W: u8 = 1;
const A: u8 = 0x0a;
const B: u8 = 0x0b;
const C: u8 = 0x0c;
const S_CLAUDE: u8 = 0x51;
const S_CODEX: u8 = 0x52;
const M_PENDING: u8 = 0x61;
const M_CLAIMED: u8 = 0x62;
const M_FAILED: u8 = 0x63;
const M_CANCELLED: u8 = 0x64;
const M_EXPIRED: u8 = 0x65;
const R_EXPIRED: u8 = 0x66;
const M_SHARED: u8 = 0x67;

/// A store as the release before the bus left it: schema V104, with mail in
/// every state the sender can see.
fn seed_a_v104_store(path: &std::path::Path) {
    let db_dir = path.join("db");
    std::fs::create_dir_all(&db_dir).unwrap();
    let mut conn = Connection::open(db_dir.join("memory.sqlite")).unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    crate::migrations::run_to(&mut conn, 104).unwrap();
    let version: i64 = conn
        .query_row(
            "SELECT MAX(version) FROM refinery_schema_history",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, 104, "the seed is the schema before the bus");
    conn.execute(
        "INSERT INTO workspaces(id, name, created_at) VALUES (?1, 'default', 1)",
        [id(W)],
    )
    .unwrap();
    for (project, name) in [(A, "api"), (B, "web"), (C, "other")] {
        conn.execute(
            "INSERT INTO projects(id, workspace_id, name, created_at) VALUES (?1, ?2, ?3, 1)",
            params![id(project), id(W), name],
        )
        .unwrap();
    }
    for (session, project, agent, started) in
        [(S_CLAUDE, A, "claude-code", 10), (S_CODEX, B, "codex", 11)]
    {
        conn.execute(
            "INSERT INTO sessions(id, workspace_id, project_id, agent_kind, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id(session), id(W), id(project), agent, started],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_registrations(workspace_id, project_id, agent_kind, source, created_at) \
             VALUES (?1, ?2, ?3, 'session', ?4)",
            params![id(W), id(project), agent, started],
        )
        .unwrap();
    }
    // (id, from, from_agent, to, to_agent, body, state, delivery_state,
    //  delivery_reason, created_at, claimed_at)
    #[allow(clippy::type_complexity)]
    let rows: [(
        u8,
        u8,
        &str,
        u8,
        Option<&str>,
        &str,
        &str,
        &str,
        Option<&str>,
        i64,
        Option<i64>,
    ); 6] = [
        (
            M_PENDING,
            A,
            "claude-code",
            B,
            Some("codex"),
            "old pending",
            "pending",
            "queued",
            None,
            100,
            None,
        ),
        (
            M_CLAIMED,
            A,
            "claude-code",
            B,
            Some("codex"),
            "old claimed",
            "claimed",
            "delivered",
            None,
            101,
            Some(150),
        ),
        (
            M_FAILED,
            B,
            "codex",
            A,
            Some("claude-code"),
            "old failed wake",
            "pending",
            "wake_failed",
            Some("session_not_found"),
            102,
            None,
        ),
        (
            M_CANCELLED,
            A,
            "claude-code",
            C,
            Some("codex"),
            "old cancelled",
            "cancelled",
            "cancelled",
            Some("sender_cancelled"),
            103,
            None,
        ),
        (
            M_EXPIRED,
            B,
            "codex",
            A,
            Some("claude-code"),
            "old expired",
            "expired",
            "expired",
            Some("ttl"),
            104,
            None,
        ),
        (
            M_SHARED,
            A,
            "claude-code",
            B,
            None,
            "old shared",
            "pending",
            "queued",
            None,
            106,
            None,
        ),
    ];
    for (mid, from, from_agent, to, to_agent, body, state, delivery, reason, created, claimed) in
        rows
    {
        conn.execute(
            "INSERT INTO agent_messages(id, from_workspace_id, from_project_id, from_agent, \
                 to_workspace_id, to_project_id, to_agent, subject, body, state, created_at, \
                 claimed_at, claimed_by_agent, notify, delivery_state, delivery_reason) \
             VALUES (?1, ?2, ?3, ?4, ?2, ?5, ?6, 'old subject', ?7, ?8, ?9, ?10, ?11, 'failure,delay', ?12, ?13)",
            params![
                id(mid),
                id(W),
                id(from),
                from_agent,
                id(to),
                to_agent,
                body,
                state,
                created,
                claimed,
                claimed.map(|_| "codex"),
                delivery,
                reason,
            ],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO agent_messages(id, from_workspace_id, from_project_id, from_agent, \
             to_workspace_id, to_project_id, to_agent, subject, body, state, created_at, kind, \
             notify, delivery_state, receipt_for, receipt_outcome, receipt_reason) \
         VALUES (?1, ?2, ?3, 'other', ?2, ?4, NULL, 'old receipt', 'old receipt body', 'pending', \
                 105, 'receipt', 'never', 'queued', ?5, 'expired', 'ttl')",
        params![id(R_EXPIRED), id(W), id(B), id(A), id(M_EXPIRED)],
    )
    .unwrap();
    for (event_id, message, at, event, reason) in [
        (
            11_i64,
            M_FAILED,
            200_i64,
            "wake_failed",
            Some("session_not_found"),
        ),
        (12, M_CLAIMED, 151, "delivered", None),
    ] {
        conn.execute(
            "INSERT INTO message_delivery_events(id, message_id, at, event, reason) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, id(message), at, event, reason],
        )
        .unwrap();
    }
}

type Row = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
);

fn read_rows(conn: &Connection) -> Vec<(u8, Row)> {
    conn.prepare(
        "SELECT id, body, state, delivery_state, delivery_reason, notify, kind, receipt_outcome, \
                receipt_reason, hop FROM agent_messages ORDER BY created_at",
    )
    .unwrap()
    .query_map([], |r| {
        let blob: Vec<u8> = r.get(0)?;
        Ok((
            blob[0],
            (
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ),
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[tokio::test]
async fn agent_bus_regression_a_v104_store_upgrades_to_v109_keeping_every_message() {
    let tmp = tempfile::tempdir().unwrap();
    seed_a_v104_store(tmp.path());

    // Opening runs V105 to V109.
    let store = Store::open(tmp.path()).unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    let version: i64 = conn
        .query_row(
            "SELECT MAX(version) FROM refinery_schema_history",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(version >= 109, "opened at V{version}");

    let row = |body: &str, state: &str, delivery: &str, reason: Option<&str>| -> Row {
        (
            body.into(),
            state.into(),
            delivery.into(),
            reason.map(str::to_owned),
            "failure,delay".into(),
            "message".into(),
            None,
            None,
            0,
        )
    };
    let mut receipt = row("old receipt body", "pending", "queued", None);
    receipt.4 = "never".into();
    receipt.5 = "receipt".into();
    receipt.6 = Some("expired".into());
    receipt.7 = Some("ttl".into());
    assert_eq!(
        read_rows(&conn),
        vec![
            (M_PENDING, row("old pending", "pending", "queued", None)),
            (M_CLAIMED, row("old claimed", "claimed", "delivered", None)),
            (
                M_FAILED,
                row(
                    "old failed wake",
                    "pending",
                    "wake_failed",
                    Some("session_not_found")
                )
            ),
            (
                M_CANCELLED,
                row(
                    "old cancelled",
                    "cancelled",
                    "cancelled",
                    Some("sender_cancelled")
                )
            ),
            (
                M_EXPIRED,
                row("old expired", "expired", "expired", Some("ttl"))
            ),
            (R_EXPIRED, receipt),
            (M_SHARED, row("old shared", "pending", "queued", None)),
        ],
        "every old message keeps its text, state and delivery; hop starts at zero"
    );

    // The timeline kept its ids and values.
    let events: Vec<(i64, i64, String, Option<String>)> = conn
        .prepare("SELECT id, at, event, reason FROM message_delivery_events ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        events,
        vec![
            (
                11,
                200,
                "wake_failed".into(),
                Some("session_not_found".into())
            ),
            (12, 151, "delivered".into(), None),
        ]
    );
    // The receipt still points at its message.
    let target: Vec<u8> = conn
        .query_row(
            "SELECT receipt_for FROM agent_messages WHERE kind = 'receipt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target, id(M_EXPIRED));
    let registry: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_registrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(registry, 2);

    // The bus tables exist, empty, with their constraints.
    for table in [
        "agent_wake_handles",
        "agent_human_anchors",
        "agent_pair_pauses",
        "agent_rooms",
        "agent_room_members",
        "agent_room_posts",
        "agent_links",
        "agent_link_events",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table} starts empty");
    }
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    assert!(
        conn.execute(
            "INSERT INTO agent_links(id, from_workspace_id, from_project_id, to_workspace_id, \
                 to_project_id, state, reason, valid_secs, requested_at, requested_by, expires_at) \
             VALUES (randomblob(16), ?1, ?2, ?1, ?2, 'requested', 'x', 300, 1, 'human', 2)",
            params![id(W), id(A)],
        )
        .is_err(),
        "a link joins two different projects"
    );
    assert!(
        conn.execute(
            "INSERT INTO agent_wake_handles(session_id, workspace_id, project_id, agent_kind, \
                 handle_kind, handle, first_seen_at, last_seen_at) \
             VALUES (?1, ?2, ?3, 'codex', 'shell', ?4, 1, 1)",
            params![
                id(S_CODEX),
                id(W),
                id(B),
                "0190f0a4-5c1e-7a2b-9c3d-4e5f60718291"
            ],
        )
        .is_err(),
        "a handle kind outside the three"
    );
    let dangling: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(dangling, 0, "the rebuilt tables point at nothing missing");
    drop(conn);

    // The new code reads the old mail as the old sender and recipient saw it.
    let (a, b) = (scope(W, A), scope(W, B));
    let outbox = store
        .reader
        .list_messages_filtered(
            b.0,
            b.1,
            MessageBox::Outbox,
            ai_memory_store::MessageListFilter {
                include_settled: true,
                message_id: Some(MessageId::from_slice(&id(M_FAILED)).unwrap()),
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(outbox[0].delivery.state, DeliveryState::WakeFailed);
    assert_eq!(
        outbox[0].delivery.reason,
        Some(DeliveryReason::SessionNotFound)
    );
    let waiting = store
        .reader
        .wake_candidates(b.0, b.1, AgentKind::Codex, false, 50)
        .await
        .unwrap();
    assert_eq!(
        waiting.len(),
        1,
        "only the pending addressed mail wakes: {waiting:?}"
    );
    assert_eq!(
        waiting[0].id,
        MessageId::from_slice(&id(M_PENDING)).unwrap()
    );
    assert!(!waiting[0].linked && !waiting[0].pair_paused);
    let receipts: Vec<_> = store
        .reader
        .list_messages(a.0, a.1, MessageBox::Inbox, 50)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.kind == MessageKind::Receipt)
        .collect();
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].receipt.as_ref().unwrap().outcome,
        ReceiptOutcome::Expired
    );
    assert!(
        store
            .reader
            .paused_agent_pairs(10)
            .await
            .unwrap()
            .is_empty()
    );

    // Old mail can be popped once, and an old claimed one never again.
    let popped = store
        .writer
        .pop_message(
            claim(b, AgentKind::Codex),
            Some(MessageId::from_slice(&id(M_PENDING)).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(popped.body, "old pending");
    assert_eq!(popped.delivery.state, DeliveryState::Delivered);
    for (at, agent, already) in [
        (b, AgentKind::Codex, M_PENDING),
        (b, AgentKind::Codex, M_CLAIMED),
        (a, AgentKind::ClaudeCode, M_EXPIRED),
    ] {
        assert!(
            store
                .writer
                .pop_message(
                    claim(at, agent),
                    Some(MessageId::from_slice(&id(already)).unwrap())
                )
                .await
                .unwrap()
                .is_none(),
            "message {already:#x} is not claimable"
        );
    }
    let state = store
        .reader
        .list_messages_filtered(
            a.0,
            a.1,
            MessageBox::Outbox,
            ai_memory_store::MessageListFilter {
                include_settled: true,
                message_id: Some(MessageId::from_slice(&id(M_CANCELLED)).unwrap()),
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(state[0].state, MessageState::Cancelled);

    // The new features work on the upgraded file: a new message starts the
    // chain at hop zero and a link can be asked.
    let fresh = send(
        &store,
        a,
        AgentKind::ClaudeCode,
        b,
        AgentKind::Codex,
        "after upgrade",
    )
    .await;
    let hop: i64 = Connection::open(store.db_path())
        .unwrap()
        .query_row(
            "SELECT hop FROM agent_messages WHERE id = ?1",
            [&fresh.as_bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hop, 0);
    store
        .writer
        .link_request(human_request(a, b, true))
        .await
        .unwrap();
}

#[tokio::test]
async fn agent_bus_regression_opening_an_upgraded_store_again_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    seed_a_v104_store(tmp.path());
    let first = {
        let store = Store::open(tmp.path()).unwrap();
        let rows = read_rows(&Connection::open(store.db_path()).unwrap());
        store.close().await;
        rows
    };
    let store = Store::open(tmp.path()).unwrap();
    let again = read_rows(&Connection::open(store.db_path()).unwrap());
    assert_eq!(first, again, "the migration set is a no-op the second time");
    assert_eq!(first.len(), 7);
}
