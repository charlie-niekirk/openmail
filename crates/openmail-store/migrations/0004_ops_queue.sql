-- The offline ops queue, reworked before anything wrote to it (no build has):
--
-- - AUTOINCREMENT, so an op's ID is never reused and stays a stable handle for
--   undo, even after earlier ops are sent and deleted.
-- - state: 'queued' (waiting, or waiting to retry), 'sending' (claimed by the
--   replay worker; undo no longer applies) or 'sent' (sent; kept until history
--   sync has caught up past sent_at, so a stale refetch cannot undo it).
DROP TABLE pending_ops;

CREATE TABLE pending_ops (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    op_type       TEXT NOT NULL,
    payload       TEXT NOT NULL CHECK (json_valid(payload)),
    state         TEXT NOT NULL DEFAULT 'queued'
        CHECK (state IN ('queued', 'sending', 'sent')),
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    sent_at       INTEGER,
    last_error    TEXT
) STRICT;

CREATE INDEX pending_ops_by_state ON pending_ops (state, next_retry_at, id);
