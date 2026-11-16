-- Rooms: named chats among the agents of one project (docs/design-agent-bus.md).
--
-- A room belongs to one project and is named by a short lowercase label. Its
-- members are harnesses of that project, the same identity addressed mail
-- uses (`agent_messages.to_agent`). A post is kept in the room's history and
-- read by each member from its own `read_seq`; it never enters an inbox. Only
-- an @mention of a harness becomes inbox mail, addressed to that harness with
-- a fixed body that points at the post, so the wake path, budgets and sender
-- gate are the ones addressed mail already has.
--
-- History is bounded: the writer keeps the newest 500 posts per room and
-- deletes older ones in the same transaction as the post that passes the cap.
-- Joins, leaves, posts and mentions are written to `audit_log` without the
-- post text. Times are unix microseconds. A deleted project takes its rooms,
-- members and posts with it.

CREATE TABLE agent_rooms (
    id            BLOB PRIMARY KEY NOT NULL,
    workspace_id  BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id    BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    name          TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 40),
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_seq      INTEGER NOT NULL DEFAULT 0,
    UNIQUE (workspace_id, project_id, name)
);

CREATE TABLE agent_room_members (
    room_id     BLOB NOT NULL REFERENCES agent_rooms(id) ON DELETE CASCADE,
    agent_kind  TEXT NOT NULL,
    joined_at   INTEGER NOT NULL,
    read_seq    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (room_id, agent_kind)
);

CREATE TABLE agent_room_posts (
    room_id          BLOB NOT NULL REFERENCES agent_rooms(id) ON DELETE CASCADE,
    seq              INTEGER NOT NULL,
    from_agent       TEXT NOT NULL,
    from_session_id  BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    body             TEXT NOT NULL,
    mentions         TEXT NOT NULL DEFAULT '',
    created_at       INTEGER NOT NULL,
    PRIMARY KEY (room_id, seq)
);
