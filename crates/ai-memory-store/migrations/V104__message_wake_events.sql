-- Delivery timeline events reported by the host wake supervisor
-- (`ai-memory message supervise`, docs/agent-messaging.md).
--
-- V103 gave `agent_messages` the full delivery-state vocabulary, but the
-- timeline's `event` CHECK listed only what the server itself records. The
-- supervisor's reports add one event per wake attempt or refusal, so the
-- CHECK gains `wake_attempted`, `wake_failed`, `agent_unavailable` and
-- `quota_exhausted`. SQLite cannot alter a CHECK constraint, so the table is
-- rebuilt; every existing row is kept with its id.

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
                      'rate_limited','auth_failed','sender_unverified'))
);

INSERT INTO message_delivery_events_new (id, message_id, at, event, reason)
SELECT id, message_id, at, event, reason FROM message_delivery_events;

DROP TABLE message_delivery_events;

ALTER TABLE message_delivery_events_new RENAME TO message_delivery_events;

CREATE INDEX idx_message_delivery_events_message
    ON message_delivery_events(message_id, at);
