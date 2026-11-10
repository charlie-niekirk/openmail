-- Threads listed during the current initial sync or full resync. When the sync
-- finishes, stored threads in its date range that it did not list are gone
-- from the server and are deleted; then the table is cleared.
CREATE TABLE sync_seen_threads (
    thread_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;
