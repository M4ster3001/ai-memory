-- Links between two projects (docs/agent-bus/CROSS_PROJECT.md).
--
-- Mail between two projects is delivered but wakes nobody, unless a link
-- joins them. A link is requested by a person (the root user, on the web
-- page) or by an agent, and the destination project confirms it: the root
-- user on the destination's page, or an agent of the destination after its
-- user answered. While it is `active`, mail between exactly those two
-- projects wakes like mail inside one project. It ends at `expires_at`, when
-- either side closes it, or when a project is purged.
--
-- A link is stored in the direction it was asked: `from_*` asked, `to_*` is
-- the destination that confirms. One live link (requested or active) exists
-- per pair of projects, in either direction; project ids are globally
-- unique, so the index orders the two ids.
--
-- `expires_at` is the end of the request while `requested` (24 hours after
-- it was filed) and the end of the link once `active` (validity counted
-- from the confirmation). The wake gate compares it with the clock itself,
-- so a link whose sweep has not run yet still never wakes anyone.
--
-- `presented_*` records the destination agent session that was shown the
-- request (`memory_link_list`); an agent's confirmation needs a user prompt
-- in that session after `presented_at`. Times are unix microseconds. No
-- message text is stored here or in the events.

CREATE TABLE agent_links (
    id                    BLOB PRIMARY KEY NOT NULL,
    from_workspace_id     BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    from_project_id       BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    to_workspace_id       BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    to_project_id         BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    state                 TEXT NOT NULL
                            CHECK (state IN ('requested','active','closed','expired')),
    reason                TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 1200),
    valid_secs            INTEGER NOT NULL CHECK (valid_secs BETWEEN 300 AND 86400),
    requested_at          INTEGER NOT NULL,
    requested_by          TEXT NOT NULL CHECK (requested_by IN ('human','agent')),
    requested_agent       TEXT,
    requested_session     BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    presented_at          INTEGER,
    presented_session     BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    confirmed_at          INTEGER,
    confirmed_via         TEXT CHECK (confirmed_via IS NULL
                                      OR confirmed_via IN ('web','web_both','agent')),
    confirmed_agent       TEXT,
    confirmed_session     BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    expires_at            INTEGER NOT NULL,
    closed_at             INTEGER,
    closed_by             TEXT,
    close_reason          TEXT CHECK (close_reason IS NULL
                                      OR close_reason IN ('closed','expired','project_purged')),
    CHECK (from_project_id <> to_project_id),
    CHECK ((state IN ('closed','expired')) = (closed_at IS NOT NULL)),
    CHECK ((state = 'active') = (confirmed_at IS NOT NULL AND closed_at IS NULL))
);

-- One live link per pair of projects, whichever side asked.
CREATE UNIQUE INDEX idx_agent_links_one_live_pair
    ON agent_links(MIN(from_project_id, to_project_id), MAX(from_project_id, to_project_id))
    WHERE state IN ('requested','active');

CREATE INDEX idx_agent_links_from ON agent_links(from_project_id, state);
CREATE INDEX idx_agent_links_to ON agent_links(to_project_id, state);
CREATE INDEX idx_agent_links_expiry ON agent_links(expires_at) WHERE state IN ('requested','active');
CREATE INDEX idx_agent_links_requested_session
    ON agent_links(requested_session) WHERE requested_session IS NOT NULL;
CREATE INDEX idx_agent_links_presented_session
    ON agent_links(presented_session) WHERE presented_session IS NOT NULL;
CREATE INDEX idx_agent_links_confirmed_session
    ON agent_links(confirmed_session) WHERE confirmed_session IS NOT NULL;

-- The audit trail of a link: what happened and when, never message text.
-- `detail` carries only enumerated words and numbers (a refusal code, a
-- wake outcome, a validity).
CREATE TABLE agent_link_events (
    id       INTEGER PRIMARY KEY,
    link_id  BLOB NOT NULL REFERENCES agent_links(id) ON DELETE CASCADE,
    at       INTEGER NOT NULL,
    event    TEXT NOT NULL
               CHECK (event IN ('requested','presented','confirmed','confirm_refused',
                                'closed','expired','wake')),
    actor    TEXT NOT NULL,
    detail   TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_agent_link_events_link ON agent_link_events(link_id, at);
