//! Response counters and the session/agent totals consumed by the dashboard.

use ai_memory_core::{
    AgentKind, NewSession, OwnerFilter, ProjectId, SessionId, TokenCall, WorkspaceId,
};
use ai_memory_store::Store;

async fn session(
    store: &Store,
    ws: WorkspaceId,
    project: ProjectId,
    agent_kind: AgentKind,
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id,
            workspace_id: ws,
            project_id: project,
            agent_kind,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    id
}

fn call(input: u64, output: u64) -> TokenCall {
    TokenCall {
        call_id: "same-native-response-id".into(),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 20,
        cache_write_tokens: 10,
        model: Some("fixture-model".into()),
    }
}

#[tokio::test]
async fn token_calls_keep_partial_reports_monotonic_in_rows_and_aggregates() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let sid = session(&store, ws, project, AgentKind::Codex).await;
    store
        .writer
        .upsert_token_calls(sid, vec![call(100, 30)])
        .await
        .unwrap();
    let mut partial = call(120, 10);
    partial.cache_read_tokens = 25;
    partial.cache_write_tokens = 5;
    partial.model = None;
    store
        .writer
        .upsert_token_calls(sid, vec![partial.clone(), partial])
        .await
        .unwrap();
    store
        .writer
        .upsert_token_calls(sid, vec![call(80, 5)])
        .await
        .unwrap();

    let report = store
        .reader
        .session_token_usage(ws, project, sid, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(
        (
            report.call_count,
            report.input,
            report.output,
            report.cache_read,
            report.cache_write
        ),
        (1, 120, 30, 25, 10)
    );
    assert_eq!(report.calls.len(), 1);
    let row = &report.calls[0];
    assert_eq!(
        (row.input, row.output, row.cache_read, row.cache_write),
        (120, 30, 25, 10)
    );
    assert_eq!(row.model, "fixture-model");
    let aggregate = store
        .reader
        .total_session_usage(ws, project, OwnerFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            aggregate.input_tokens,
            aggregate.output_tokens,
            aggregate.cache_read_tokens,
            aggregate.cache_write_tokens
        ),
        (120, 30, 25, 10)
    );
    let current = store
        .reader
        .session_summary_scoped(ws, project, sid, OwnerFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert!(
        current.ended_at.is_none(),
        "Live reporting must not depend on session-end"
    );
}

#[tokio::test]
async fn token_calls_group_by_session_and_agent_without_merging_foreign_responses() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let mut local = Vec::new();
    for (scope, agent, input) in [
        (project, AgentKind::Codex, 100),
        (project, AgentKind::Codex, 200),
        (project, AgentKind::ClaudeCode, 300),
        (other, AgentKind::Codex, 900),
    ] {
        let sid = session(&store, ws, scope, agent).await;
        // Native ids are only unique within a session; replay each report too.
        store
            .writer
            .upsert_token_calls(sid, vec![call(input, 20), call(input, 20)])
            .await
            .unwrap();
        if scope == project {
            local.push((sid, input));
        }
    }
    let project_usage = store
        .reader
        .token_usage_report(ws, project, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(
        (
            project_usage.call_count,
            project_usage.input,
            project_usage.output
        ),
        (3, 600, 60)
    );
    for (sid, input) in local {
        let ledger = store
            .reader
            .session_token_usage(ws, project, sid, OwnerFilter::Any)
            .await
            .unwrap();
        assert_eq!(
            (ledger.call_count, ledger.input, ledger.output),
            (1, input, 20)
        );
    }
    let agents = store
        .reader
        .agent_usage(ws, project, OwnerFilter::Any)
        .await
        .unwrap();
    let codex = agents.iter().find(|a| a.agent == "codex").unwrap();
    assert_eq!(
        (
            codex.sessions,
            codex.measured_sessions,
            codex.calls,
            codex.input,
            codex.output
        ),
        (2, 2, 2, 300, 40)
    );
    let claude = agents.iter().find(|a| a.agent == "claude-code").unwrap();
    assert_eq!((claude.sessions, claude.calls, claude.input), (1, 1, 300));
    let foreign = store
        .reader
        .token_usage_report(ws, other, OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!((foreign.call_count, foreign.input), (1, 900));
}
