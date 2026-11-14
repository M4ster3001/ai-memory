-- Harnesses known to each project, for addressed agent mail.
--
-- A row means "this harness has started a session in this project". It is
-- routing, not authentication: it decides whether mail addressed to the
-- harness (`agent_messages.to_agent`) is `queued` or `awaiting_agent` for its
-- sender, and nothing else. Who may pop addressed mail is unchanged.
--
-- The writer inserts a row in the same transaction that creates the first
-- session of a harness in a project (`source = 'session'`). Registration by an
-- operator command is a later stage of
-- docs/design-agent-wakeup-and-secure-messaging.md and will widen `source`.
-- `agent_kind` has no CHECK: the sessions CHECK already bounds what can be
-- inserted here, and repeating it would force a rebuild of this table for
-- every new harness.

CREATE TABLE agent_registrations (
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    agent_kind    TEXT NOT NULL,
    source        TEXT NOT NULL CHECK (source IN ('session')),
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, agent_kind)
);

-- Every harness that already ran somewhere stays addressable there.
INSERT INTO agent_registrations (workspace_id, project_id, agent_kind, source, created_at)
SELECT workspace_id, project_id, agent_kind, 'session', MIN(started_at)
FROM sessions
GROUP BY workspace_id, project_id, agent_kind;
