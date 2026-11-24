CREATE TABLE IF NOT EXISTS comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  slug TEXT NOT NULL,                 -- Normalized pathname, e.g. "/posts/hello-world/"
  parent_id INTEGER DEFAULT NULL,     -- NULL = top-level comment; integer = root comment id
  reply_to_author TEXT DEFAULT NULL,  -- Target username for @mention display in nested threads
  author TEXT NOT NULL,
  is_admin INTEGER DEFAULT 0,         -- 1 = site administrator / author, 0 = visitor
  tg_message_id INTEGER DEFAULT NULL, -- Telegram message_id for reply matching
  content_html TEXT NOT NULL,         -- Sanitized HTML rendered from Markdown
  created_at INTEGER NOT NULL,        -- Unix timestamp (seconds)
  status INTEGER DEFAULT 1            -- 1 = published, 0 = pending/hidden
);

CREATE INDEX IF NOT EXISTS idx_comments_slug_id ON comments(slug, status, id);
CREATE INDEX IF NOT EXISTS idx_comments_parent_id ON comments(parent_id, status, id);
CREATE INDEX IF NOT EXISTS idx_comments_tg_msg ON comments(tg_message_id);
