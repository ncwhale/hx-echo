-- Baseline D1 schema for hx-echo (idempotent for fresh and existing installs)
CREATE TABLE IF NOT EXISTS comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  slug TEXT NOT NULL,
  parent_id INTEGER DEFAULT NULL,
  reply_to_author TEXT DEFAULT NULL,
  author TEXT NOT NULL,
  content_html TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  status INTEGER DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_comments_slug_id ON comments(slug, status, id);
CREATE INDEX IF NOT EXISTS idx_comments_parent_id ON comments(parent_id, status, id);
