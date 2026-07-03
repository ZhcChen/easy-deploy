PRAGMA foreign_keys = OFF;

CREATE TABLE app_release_queue_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    release_id INTEGER NOT NULL REFERENCES app_releases(id) ON DELETE CASCADE,
    config_snapshot_id INTEGER REFERENCES app_config_snapshots(id) ON DELETE SET NULL,
    queue_seq INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('scheduled', 'queued', 'running', 'success', 'failed', 'canceled')),
    triggered_by TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    task_id INTEGER REFERENCES operation_tasks(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    scheduled_publish_at TEXT
);

INSERT INTO app_release_queue_new (
    id,
    app_id,
    release_id,
    config_snapshot_id,
    queue_seq,
    status,
    triggered_by,
    message,
    task_id,
    created_at,
    started_at,
    finished_at,
    updated_at,
    scheduled_publish_at
)
SELECT
    id,
    app_id,
    release_id,
    config_snapshot_id,
    queue_seq,
    status,
    triggered_by,
    message,
    task_id,
    created_at,
    started_at,
    finished_at,
    updated_at,
    (
        SELECT scheduled_publish_at
        FROM app_releases
        WHERE app_releases.id = app_release_queue.release_id
    )
FROM app_release_queue;

DROP TABLE app_release_queue;
ALTER TABLE app_release_queue_new RENAME TO app_release_queue;

CREATE INDEX IF NOT EXISTS idx_app_release_queue_app_seq
ON app_release_queue(app_id, queue_seq ASC, id ASC);

CREATE INDEX IF NOT EXISTS idx_app_release_queue_status_seq
ON app_release_queue(status, queue_seq ASC, id ASC);

CREATE UNIQUE INDEX IF NOT EXISTS idx_app_release_queue_unique_active_release
ON app_release_queue(release_id)
WHERE status IN ('queued', 'running');

CREATE UNIQUE INDEX IF NOT EXISTS idx_app_release_queue_single_running_app
ON app_release_queue(app_id)
WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_app_release_queue_scheduled_publish
ON app_release_queue(scheduled_publish_at, status, id);

PRAGMA foreign_keys = ON;
