CREATE TABLE IF NOT EXISTS app_health_checks (
    app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
    check_kind TEXT NOT NULL DEFAULT 'none' CHECK (check_kind IN ('none', 'http', 'tcp', 'compose_running')),
    endpoint TEXT NOT NULL DEFAULT '',
    timeout_secs INTEGER NOT NULL DEFAULT 5,
    expected_status INTEGER NOT NULL DEFAULT 200,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
