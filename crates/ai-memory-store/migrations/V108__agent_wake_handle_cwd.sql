-- The directory a wake handle's session reported last (docs/agent-bus/).
--
-- `sessions.cwd` keeps the directory a session was admitted in, so a Codex
-- conversation that moved on, or whose first directory was deleted, kept
-- pointing at the old one, and a headless resume there failed with
-- `session_not_found` until someone wrote a supervisor entry by hand. Every
-- hook event that refreshes a handle now records the directory it reported,
-- and the bus answers that one, so the next prompt in the session repairs it.
-- NULL until the session's next hook event; the bus falls back to
-- `sessions.cwd` meanwhile.

ALTER TABLE agent_wake_handles ADD COLUMN cwd TEXT CHECK (cwd IS NULL OR length(cwd) <= 4096);
