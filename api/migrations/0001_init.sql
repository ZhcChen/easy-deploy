CREATE TABLE IF NOT EXISTS schema_markers (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL
);

INSERT OR IGNORE INTO schema_markers(key, value, created_at)
VALUES('initial_schema', 'ok', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
