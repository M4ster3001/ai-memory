-- How the agent bus can wake a harness session (docs/design-agent-bus.md).
--
-- A row is written by hook ingest, in the same transaction as the event that
-- reports the session, when the harness has a wake handle and the session id
-- it sent is a canonical UUID: the Claude Code session id (`claude --resume`)
-- or the Codex thread id (`codex queue --thread`). Any other harness, or a
-- session id the server had to hash, gets no row, and no row means no wake:
-- the bus never guesses a handle.
--
-- `handle` is the session id as the harness spelled it. It is not a secret
-- (the sessions table holds the same UUID), and it never reaches a model:
-- only the host daemon reads it, to build an adapter's argv.
--
-- Whether the session is still open is read from `sessions.ended_at`, so
-- every way a session ends (a SessionEnd hook, the idle auto-end, the
-- abandoned-session sweep) and every way it reopens is seen without a second
-- copy of that state. Times are unix microseconds, like `sessions`. A purged
-- session takes its row with it.
--
-- Existing sessions get no row: their hook events after the upgrade add one,
-- so the first sign of the bus is a session that reports itself, never a
-- handle inferred from old data.

CREATE TABLE agent_wake_handles (
    session_id    BLOB PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    agent_kind    TEXT NOT NULL,
    handle_kind   TEXT NOT NULL CHECK (handle_kind IN ('claude_session','codex_thread')),
    handle        TEXT NOT NULL CHECK (length(handle) = 36),
    first_seen_at INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL
);

-- The daemon picks the newest handle of a harness in a project.
CREATE INDEX idx_agent_wake_handles_agent
    ON agent_wake_handles(workspace_id, project_id, agent_kind, last_seen_at DESC);
