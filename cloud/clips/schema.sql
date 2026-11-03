CREATE TABLE IF NOT EXISTS clips (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  size INTEGER NOT NULL,
  title TEXT NOT NULL,
  game TEXT NOT NULL,
  duration REAL NOT NULL,
  width INTEGER NOT NULL,
  height INTEGER NOT NULL,
  has_thumb INTEGER NOT NULL DEFAULT 0,
  delete_hash TEXT NOT NULL,
  hidden INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS clips_expiry ON clips (expires_at);

-- One reaction per browser per clip. The voter is a hash, not an identity.
CREATE TABLE IF NOT EXISTS reactions (
  clip_id TEXT NOT NULL,
  voter TEXT NOT NULL,
  emoji TEXT NOT NULL,
  PRIMARY KEY (clip_id, voter)
);

CREATE TABLE IF NOT EXISTS reports (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  clip_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  reason TEXT NOT NULL,
  detail TEXT NOT NULL,
  contact TEXT NOT NULL,
  reporter TEXT NOT NULL,
  resolved INTEGER NOT NULL DEFAULT 0,
  UNIQUE (clip_id, reporter)
);

-- The hard limits. Each row is updated in one conditional statement, so two
-- uploads at once can't both slip past a cap.
CREATE TABLE IF NOT EXISTS usage (id INTEGER PRIMARY KEY CHECK (id = 1), bytes INTEGER NOT NULL);
INSERT OR IGNORE INTO usage (id, bytes) VALUES (1, 0);
CREATE TABLE IF NOT EXISTS upload_days (day TEXT PRIMARY KEY, bytes INTEGER NOT NULL);
-- Hashed with a secret, and deleted after a day.
CREATE TABLE IF NOT EXISTS uploader_days (day TEXT NOT NULL, uploader TEXT NOT NULL, uploads INTEGER NOT NULL, PRIMARY KEY (day, uploader));
CREATE TABLE IF NOT EXISTS request_months (month TEXT PRIMARY KEY, requests INTEGER NOT NULL);
