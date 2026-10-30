CREATE TABLE IF NOT EXISTS reports (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  version TEXT,
  os TEXT,
  gpu TEXT,
  summary TEXT,
  contact TEXT
);
CREATE INDEX IF NOT EXISTS reports_created ON reports (created_at DESC);
