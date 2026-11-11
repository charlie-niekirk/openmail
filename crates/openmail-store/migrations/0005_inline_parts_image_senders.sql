-- Inline images: the parts a body's HTML references with `cid:` URLs, saved with
-- the body so the reading pane can show them offline. Replaced whenever the body
-- is, and deleted with it.
CREATE TABLE inline_parts (
    message_id TEXT NOT NULL REFERENCES bodies (message_id) ON DELETE CASCADE,
    content_id TEXT NOT NULL,
    mime_type  TEXT NOT NULL,
    data       BLOB NOT NULL,
    PRIMARY KEY (message_id, content_id)
) STRICT;

-- Senders whose remote images are always shown ("Always show from this sender").
-- address is lowercased.
CREATE TABLE image_senders (
    address  TEXT PRIMARY KEY,
    added_at INTEGER NOT NULL
) STRICT;

-- Bodies stored before inline parts were kept have lost them: fetch the ones that
-- reference any again when they are next opened.
DELETE FROM bodies WHERE html LIKE '%cid:%';
