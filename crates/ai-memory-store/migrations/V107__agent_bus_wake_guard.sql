-- Wakes without an hourly budget (docs/design-agent-bus.md, slice F3).
--
-- The agent bus stops counting wakes per hour. A loop between agents is
-- caught by structure instead, on the server and without a model:
--
-- - `agent_messages.hop` is how many agent turns, each woken by mail, led to
--   this message since a human last prompted its sender. 0 when the sender
--   was prompted by a human after it last claimed mail.
-- - `agent_human_anchors` keeps, per harness of a project, when a human last
--   prompted one of its sessions. A prompt that is only the fixed wake text
--   does not count.
-- - `agent_pair_pauses` holds the circuit breaker of each pair of agents
--   (harness in a project, ordered so a pair has one row). While
--   `paused_at` is set, mail between the two is still delivered but wakes
--   neither side. A human prompt in a session of either side, or a click on
--   the dashboard, resumes the pair.
--
-- `loop_suspected` joins the delivery reasons, and `gemini_session` the wake
-- handle kinds. SQLite cannot alter a CHECK constraint, so `agent_messages`,
-- `message_delivery_events` and `agent_wake_handles` are rebuilt as V103 and
-- V104 did; every row keeps its values. `Store::open` turns foreign keys off
-- around the migration runner.
--
-- Room posts gain the index their `from_session_id` foreign key lacked, so
-- deleting a session does not scan every post.

CREATE TABLE agent_messages_new (
    id                 BLOB PRIMARY KEY NOT NULL,
    from_workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    from_project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    from_agent         TEXT NOT NULL,
    from_session_id    BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    from_owner_user    TEXT,
    to_workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    to_project_id      BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    subject            TEXT,
    body               TEXT NOT NULL,
    state              TEXT NOT NULL DEFAULT 'pending'
                         CHECK (state IN ('pending','claimed','cancelled','editing','expired')),
    created_at         INTEGER NOT NULL,
    claimed_at         INTEGER,
    claimed_by_session BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    claimed_by_agent   TEXT,
    claimed_by_user    TEXT,
    nature             TEXT
                         CHECK (nature IS NULL OR nature IN ('unknown','measured','reported','inferred')),
    evidence           TEXT,
    to_agent           TEXT,
    edited_at          INTEGER,
    kind               TEXT NOT NULL DEFAULT 'message'
                         CHECK (kind IN ('message','receipt')),
    priority           TEXT NOT NULL DEFAULT 'normal'
                         CHECK (priority IN ('low','normal','high')),
    notify             TEXT NOT NULL DEFAULT 'never'
                         CHECK (notify IN ('never','failure','delay','success','failure,delay',
                                           'failure,success','delay,success','failure,delay,success')),
    expires_at         INTEGER,
    no_pop_alert_at    INTEGER,
    delivery_state     TEXT NOT NULL DEFAULT 'queued'
                         CHECK (delivery_state IN ('queued','awaiting_agent','wake_attempted','delivered',
                                                   'wake_failed','agent_unavailable','quota_exhausted',
                                                   'expired','dead','cancelled')),
    delivery_reason    TEXT
                         CHECK (delivery_reason IS NULL OR delivery_reason IN (
                             'agent_not_registered','no_pop_timeout','ttl','sender_cancelled',
                             'adapter_exit_nonzero','adapter_timeout','binary_missing','not_approved',
                             'budget_exhausted','session_busy','session_not_found','quota_marker',
                             'rate_limited','auth_failed','sender_unverified','loop_suspected')),
    retry_after        INTEGER,
    receipt_for        BLOB REFERENCES agent_messages(id) ON DELETE SET NULL,
    receipt_outcome    TEXT
                         CHECK (receipt_outcome IS NULL OR receipt_outcome IN (
                             'delivered','delayed','expired','wake_failed','agent_unavailable',
                             'quota_exhausted','dead')),
    receipt_reason     TEXT
                         CHECK (receipt_reason IS NULL OR receipt_reason IN (
                             'agent_not_registered','no_pop_timeout','ttl','sender_cancelled',
                             'adapter_exit_nonzero','adapter_timeout','binary_missing','not_approved',
                             'budget_exhausted','session_busy','session_not_found','quota_marker',
                             'rate_limited','auth_failed','sender_unverified','loop_suspected')),
    hop                INTEGER NOT NULL DEFAULT 0 CHECK (hop >= 0),
    CHECK ((kind = 'message' AND receipt_for IS NULL AND receipt_outcome IS NULL
               AND receipt_reason IS NULL)
        OR (kind = 'receipt' AND receipt_outcome IS NOT NULL AND notify = 'never'
               AND no_pop_alert_at IS NULL)),
    FOREIGN KEY (from_session_id)    REFERENCES sessions(id) ON DELETE SET NULL,
    FOREIGN KEY (claimed_by_session) REFERENCES sessions(id) ON DELETE SET NULL
);

INSERT INTO agent_messages_new (
    id, from_workspace_id, from_project_id, from_agent, from_session_id,
    from_owner_user, to_workspace_id, to_project_id, subject, body, state,
    created_at, claimed_at, claimed_by_session, claimed_by_agent,
    claimed_by_user, nature, evidence, to_agent, edited_at,
    kind, priority, notify, expires_at, no_pop_alert_at,
    delivery_state, delivery_reason, retry_after, receipt_for, receipt_outcome,
    receipt_reason
)
SELECT
    id, from_workspace_id, from_project_id, from_agent, from_session_id,
    from_owner_user, to_workspace_id, to_project_id, subject, body, state,
    created_at, claimed_at, claimed_by_session, claimed_by_agent,
    claimed_by_user, nature, evidence, to_agent, edited_at,
    kind, priority, notify, expires_at, no_pop_alert_at,
    delivery_state, delivery_reason, retry_after, receipt_for, receipt_outcome,
    receipt_reason
FROM agent_messages;

DROP TABLE agent_messages;

ALTER TABLE agent_messages_new RENAME TO agent_messages;

CREATE INDEX idx_agent_messages_inbox
    ON agent_messages(to_workspace_id, to_project_id, created_at)
    WHERE state = 'pending';

CREATE INDEX idx_agent_messages_outbox
    ON agent_messages(from_workspace_id, from_project_id, created_at)
    WHERE state = 'pending';

CREATE INDEX idx_messages_agent_inbox
    ON agent_messages(to_workspace_id, to_project_id, state, to_agent, created_at);

CREATE INDEX idx_agent_messages_expiry
    ON agent_messages(expires_at)
    WHERE state = 'pending' AND expires_at IS NOT NULL;

CREATE INDEX idx_agent_messages_no_pop_alert
    ON agent_messages(no_pop_alert_at)
    WHERE state = 'pending' AND no_pop_alert_at IS NOT NULL;

CREATE INDEX idx_agent_messages_sent_history
    ON agent_messages(from_workspace_id, from_project_id, created_at);

CREATE INDEX idx_agent_messages_awaiting_agent
    ON agent_messages(to_workspace_id, to_project_id, to_agent)
    WHERE delivery_state = 'awaiting_agent';

CREATE UNIQUE INDEX idx_agent_messages_receipt_once
    ON agent_messages(receipt_for, receipt_outcome)
    WHERE kind = 'receipt';

CREATE TABLE message_delivery_events_new (
    id          INTEGER PRIMARY KEY,
    message_id  BLOB NOT NULL REFERENCES agent_messages(id) ON DELETE CASCADE,
    at          INTEGER NOT NULL,
    event       TEXT NOT NULL
                  CHECK (event IN ('agent_registered','no_pop_alert','expired','delivered',
                                   'cancelled','receipt_sent','receipt_dropped',
                                   'wake_attempted','wake_failed','agent_unavailable',
                                   'quota_exhausted')),
    reason      TEXT
                  CHECK (reason IS NULL OR reason IN (
                      'agent_not_registered','no_pop_timeout','ttl','sender_cancelled',
                      'adapter_exit_nonzero','adapter_timeout','binary_missing','not_approved',
                      'budget_exhausted','session_busy','session_not_found','quota_marker',
                      'rate_limited','auth_failed','sender_unverified','loop_suspected'))
);

INSERT INTO message_delivery_events_new (id, message_id, at, event, reason)
SELECT id, message_id, at, event, reason FROM message_delivery_events;

DROP TABLE message_delivery_events;

ALTER TABLE message_delivery_events_new RENAME TO message_delivery_events;

CREATE INDEX idx_message_delivery_events_message
    ON message_delivery_events(message_id, at);

CREATE TABLE agent_wake_handles_new (
    session_id    BLOB PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    agent_kind    TEXT NOT NULL,
    handle_kind   TEXT NOT NULL
                    CHECK (handle_kind IN ('claude_session','codex_thread','gemini_session')),
    handle        TEXT NOT NULL CHECK (length(handle) = 36),
    first_seen_at INTEGER NOT NULL,
    last_seen_at  INTEGER NOT NULL
);

INSERT INTO agent_wake_handles_new (
    session_id, workspace_id, project_id, agent_kind, handle_kind, handle,
    first_seen_at, last_seen_at
)
SELECT session_id, workspace_id, project_id, agent_kind, handle_kind, handle,
       first_seen_at, last_seen_at
FROM agent_wake_handles;

DROP TABLE agent_wake_handles;

ALTER TABLE agent_wake_handles_new RENAME TO agent_wake_handles;

CREATE INDEX idx_agent_wake_handles_agent
    ON agent_wake_handles(workspace_id, project_id, agent_kind, last_seen_at DESC);

CREATE TABLE agent_human_anchors (
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    agent_kind      TEXT NOT NULL,
    last_prompt_at  INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, agent_kind)
);

-- Side a is the lexically smaller (workspace, project, harness) of the pair.
CREATE TABLE agent_pair_pauses (
    a_workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    a_project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    a_agent         TEXT NOT NULL,
    b_workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    b_project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    b_agent         TEXT NOT NULL,
    paused_at       INTEGER,
    reason          TEXT CHECK (reason IS NULL OR reason IN
                                ('ping_pong','repeated_content','thread_depth')),
    trip_count      INTEGER NOT NULL DEFAULT 0,
    resumed_at      INTEGER,
    resumed_via     TEXT CHECK (resumed_via IS NULL OR resumed_via IN ('human_prompt','dashboard')),
    PRIMARY KEY (a_workspace_id, a_project_id, a_agent, b_workspace_id, b_project_id, b_agent)
);

CREATE INDEX idx_agent_pair_pauses_b
    ON agent_pair_pauses(b_workspace_id, b_project_id, b_agent);

CREATE INDEX idx_agent_room_posts_session
    ON agent_room_posts(from_session_id)
    WHERE from_session_id IS NOT NULL;
