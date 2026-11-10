-- Initial schema for one account's database.
--
-- Conventions: dates are Unix milliseconds (Gmail's internalDate unit), booleans
-- are 0/1, history IDs are integers, address lists are JSON arrays of
-- {"name": …, "email": …} objects. Label IDs are not foreign keys: messages can
-- carry labels the store has not synced yet.

-- The account this database belongs to (one row).
CREATE TABLE accounts (
    id           TEXT PRIMARY KEY,
    email        TEXT NOT NULL UNIQUE,
    display_name TEXT
) STRICT;

CREATE TABLE labels (
    id                      TEXT PRIMARY KEY,
    name                    TEXT NOT NULL,
    type                    TEXT NOT NULL CHECK (type IN ('system', 'user')),
    text_color              TEXT,
    background_color        TEXT,
    label_list_visibility   TEXT NOT NULL
        CHECK (label_list_visibility IN ('labelShow', 'labelShowIfUnread', 'labelHide')),
    message_list_visibility TEXT NOT NULL
        CHECK (message_list_visibility IN ('show', 'hide'))
) STRICT;

-- Thread summaries for the thread list. is_unread and is_starred are maintained
-- from thread_labels by triggers; the other summary fields are written by sync.
CREATE TABLE threads (
    id                TEXT PRIMARY KEY,
    history_id        INTEGER NOT NULL,
    subject           TEXT,
    snippet           TEXT NOT NULL,
    last_message_date INTEGER NOT NULL,
    message_count     INTEGER NOT NULL,
    senders           TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(senders)),
    has_attachments   INTEGER NOT NULL DEFAULT 0,
    is_unread         INTEGER NOT NULL DEFAULT 0,
    is_starred        INTEGER NOT NULL DEFAULT 0
) STRICT;

-- "All Mail" style views over every thread.
CREATE INDEX threads_by_date ON threads (last_message_date DESC, id DESC);

-- Message metadata. pk is a stable integer key for the FTS index (an implicit
-- rowid could be renumbered by VACUUM).
CREATE TABLE messages (
    pk                INTEGER PRIMARY KEY,
    id                TEXT NOT NULL UNIQUE,
    thread_id         TEXT NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    history_id        INTEGER NOT NULL,
    snippet           TEXT NOT NULL,
    internal_date     INTEGER NOT NULL,
    size_estimate     INTEGER NOT NULL,
    subject           TEXT,
    from_addrs        TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(from_addrs)),
    to_addrs          TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(to_addrs)),
    cc_addrs          TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(cc_addrs)),
    bcc_addrs         TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(bcc_addrs)),
    reply_to_addrs    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(reply_to_addrs)),
    message_id_header TEXT,
    in_reply_to       TEXT,
    "references"      TEXT,
    has_attachments   INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Thread detail: a thread's messages in date order.
CREATE INDEX messages_by_thread ON messages (thread_id, internal_date);

-- Labels on each message: the source of truth for labels. thread_id is copied
-- from the message so the triggers below need no lookup.
CREATE TABLE message_labels (
    message_id TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    label_id   TEXT NOT NULL,
    thread_id  TEXT NOT NULL,
    PRIMARY KEY (message_id, label_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX message_labels_by_label ON message_labels (label_id);

-- Labels on each thread (the union of its messages' labels), maintained by
-- triggers. message_count counts the thread's messages with the label;
-- last_message_date mirrors threads.last_message_date for keyset paging.
CREATE TABLE thread_labels (
    thread_id         TEXT NOT NULL REFERENCES threads (id) ON DELETE CASCADE,
    label_id          TEXT NOT NULL,
    message_count     INTEGER NOT NULL,
    last_message_date INTEGER NOT NULL,
    PRIMARY KEY (thread_id, label_id)
) STRICT, WITHOUT ROWID;

-- A label's threads, newest first: keyset paging on (last_message_date, thread_id).
CREATE INDEX thread_labels_by_date
    ON thread_labels (label_id, last_message_date DESC, thread_id DESC);

CREATE TRIGGER message_labels_insert AFTER INSERT ON message_labels
BEGIN
    INSERT INTO thread_labels (thread_id, label_id, message_count, last_message_date)
    SELECT NEW.thread_id, NEW.label_id, 1, last_message_date
    FROM threads WHERE id = NEW.thread_id
    ON CONFLICT (thread_id, label_id) DO UPDATE SET message_count = message_count + 1;
END;

CREATE TRIGGER message_labels_delete AFTER DELETE ON message_labels
BEGIN
    UPDATE thread_labels SET message_count = message_count - 1
    WHERE thread_id = OLD.thread_id AND label_id = OLD.label_id;
    DELETE FROM thread_labels
    WHERE thread_id = OLD.thread_id AND label_id = OLD.label_id AND message_count <= 0;
END;

CREATE TRIGGER threads_date_update AFTER UPDATE OF last_message_date ON threads
WHEN OLD.last_message_date IS NOT NEW.last_message_date
BEGIN
    UPDATE thread_labels SET last_message_date = NEW.last_message_date
    WHERE thread_id = NEW.id;
END;

CREATE TRIGGER thread_labels_flags_insert AFTER INSERT ON thread_labels
WHEN NEW.label_id IN ('UNREAD', 'STARRED')
BEGIN
    UPDATE threads SET
        is_unread = is_unread OR NEW.label_id = 'UNREAD',
        is_starred = is_starred OR NEW.label_id = 'STARRED'
    WHERE id = NEW.thread_id;
END;

CREATE TRIGGER thread_labels_flags_delete AFTER DELETE ON thread_labels
WHEN OLD.label_id IN ('UNREAD', 'STARRED')
BEGIN
    UPDATE threads SET
        is_unread = is_unread AND OLD.label_id <> 'UNREAD',
        is_starred = is_starred AND OLD.label_id <> 'STARRED'
    WHERE id = OLD.thread_id;
END;

-- Message bodies, fetched lazily. text is what search indexes, so fill it (with
-- text converted from the HTML if there is no text part).
CREATE TABLE bodies (
    message_id TEXT PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE,
    text       TEXT,
    html       TEXT,
    fetched_at INTEGER NOT NULL
) STRICT;

-- Attachment metadata, keyed by MIME part ID (Gmail attachment IDs are not stable).
CREATE TABLE attachments (
    message_id    TEXT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    part_id       TEXT NOT NULL,
    attachment_id TEXT,
    filename      TEXT NOT NULL,
    mime_type     TEXT NOT NULL,
    size          INTEGER NOT NULL,
    content_id    TEXT,
    is_inline     INTEGER NOT NULL DEFAULT 0,
    -- Where the content was saved, once downloaded.
    local_path    TEXT,
    PRIMARY KEY (message_id, part_id)
) STRICT, WITHOUT ROWID;

-- Drafts: a draft ID for a message stored in messages (with the DRAFT label).
CREATE TABLE drafts (
    id         TEXT PRIMARY KEY,
    message_id TEXT NOT NULL UNIQUE REFERENCES messages (id) ON DELETE CASCADE
) STRICT;

-- Sync progress (one row). backfill_cursor is opaque to the store.
CREATE TABLE sync_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    history_id        INTEGER,
    backfill_cursor   TEXT,
    backfill_complete INTEGER NOT NULL DEFAULT 0,
    last_full_sync    INTEGER
) STRICT;

INSERT INTO sync_state (id) VALUES (1);

-- Local changes waiting to be sent to Gmail, replayed in id order.
CREATE TABLE pending_ops (
    id            INTEGER PRIMARY KEY,
    op_type       TEXT NOT NULL,
    payload       TEXT NOT NULL CHECK (json_valid(payload)),
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    last_error    TEXT
) STRICT;

CREATE INDEX pending_ops_due ON pending_ops (next_retry_at, id);

-- Full-text search over messages. Contentless (the text already lives in messages
-- and bodies), with rowid = messages.pk. Rows are rebuilt by the triggers below
-- from messages_fts_source.
CREATE VIRTUAL TABLE messages_fts USING fts5 (
    subject,
    sender,
    recipients,
    body,
    content = '',
    contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- What messages_fts indexes for each message: names and addresses as plain text.
CREATE VIEW messages_fts_source AS
SELECT
    m.pk AS pk,
    m.id AS id,
    m.subject AS subject,
    (SELECT group_concat(concat_ws(' ', a.value ->> 'name', a.value ->> 'email'), ' ')
     FROM json_each(m.from_addrs) AS a) AS sender,
    (SELECT group_concat(concat_ws(' ', a.value ->> 'name', a.value ->> 'email'), ' ')
     FROM (SELECT value FROM json_each(m.to_addrs)
           UNION ALL SELECT value FROM json_each(m.cc_addrs)
           UNION ALL SELECT value FROM json_each(m.bcc_addrs)) AS a) AS recipients,
    b.text AS body
FROM messages AS m
LEFT JOIN bodies AS b ON b.message_id = m.id;

CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages
BEGIN
    INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
    SELECT pk, subject, sender, recipients, body FROM messages_fts_source WHERE pk = NEW.pk;
END;

CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages
WHEN OLD.subject IS NOT NEW.subject
    OR OLD.from_addrs IS NOT NEW.from_addrs
    OR OLD.to_addrs IS NOT NEW.to_addrs
    OR OLD.cc_addrs IS NOT NEW.cc_addrs
    OR OLD.bcc_addrs IS NOT NEW.bcc_addrs
BEGIN
    DELETE FROM messages_fts WHERE rowid = OLD.pk;
    INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
    SELECT pk, subject, sender, recipients, body FROM messages_fts_source WHERE pk = NEW.pk;
END;

CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages
BEGIN
    DELETE FROM messages_fts WHERE rowid = OLD.pk;
END;

CREATE TRIGGER bodies_fts_insert AFTER INSERT ON bodies
BEGIN
    DELETE FROM messages_fts WHERE rowid = (SELECT pk FROM messages WHERE id = NEW.message_id);
    INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
    SELECT pk, subject, sender, recipients, body FROM messages_fts_source WHERE id = NEW.message_id;
END;

CREATE TRIGGER bodies_fts_update AFTER UPDATE OF text ON bodies
WHEN OLD.text IS NOT NEW.text
BEGIN
    DELETE FROM messages_fts WHERE rowid = (SELECT pk FROM messages WHERE id = NEW.message_id);
    INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
    SELECT pk, subject, sender, recipients, body FROM messages_fts_source WHERE id = NEW.message_id;
END;

CREATE TRIGGER bodies_fts_delete AFTER DELETE ON bodies
BEGIN
    DELETE FROM messages_fts WHERE rowid = (SELECT pk FROM messages WHERE id = OLD.message_id);
    INSERT INTO messages_fts (rowid, subject, sender, recipients, body)
    SELECT pk, subject, sender, recipients, body FROM messages_fts_source WHERE id = OLD.message_id;
END;
