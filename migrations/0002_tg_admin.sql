-- Add Telegram bidirectional admin flags and message tracking
ALTER TABLE comments ADD COLUMN is_admin INTEGER DEFAULT 0;
ALTER TABLE comments ADD COLUMN tg_message_id INTEGER DEFAULT NULL;
CREATE INDEX IF NOT EXISTS idx_comments_tg_msg ON comments(tg_message_id);
