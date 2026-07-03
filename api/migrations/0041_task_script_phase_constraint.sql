-- Add migration script here.
PRAGMA foreign_keys = OFF;

CREATE TABLE operation_tasks_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_kind TEXT NOT NULL,
    title TEXT NOT NULL,
    app_id INTEGER REFERENCES apps(id) ON DELETE SET NULL,
    node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'success', 'failed', 'canceled')),
    command TEXT NOT NULL DEFAULT '',
    summary TEXT NOT NULL DEFAULT '',
    exit_code INTEGER,
    started_at TEXT,
    finished_at TEXT,
    created_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    phase TEXT NOT NULL DEFAULT 'queued'
        CHECK (
            phase IN (
                'queued',
                'preflight',
                'preparing_files',
                'executing',
                'healthchecking',
                'prepare',
                'render',
                'pre_deploy',
                'deploy',
                'post_deploy',
                'switch_traffic',
                'cleanup',
                'finalize',
                'completed',
                'failed',
                'canceled'
            )
        ),
    release_id INTEGER REFERENCES app_releases(id) ON DELETE SET NULL
);

INSERT INTO operation_tasks_new (
    id,
    task_kind,
    title,
    app_id,
    node_id,
    status,
    command,
    summary,
    exit_code,
    started_at,
    finished_at,
    created_by,
    created_at,
    updated_at,
    phase,
    release_id
)
SELECT
    id,
    task_kind,
    title,
    app_id,
    node_id,
    status,
    command,
    summary,
    exit_code,
    started_at,
    finished_at,
    created_by,
    created_at,
    updated_at,
    phase,
    release_id
FROM operation_tasks;

DROP TABLE operation_tasks;
ALTER TABLE operation_tasks_new RENAME TO operation_tasks;

CREATE INDEX IF NOT EXISTS idx_operation_tasks_status
ON operation_tasks(status, created_at);

CREATE INDEX IF NOT EXISTS idx_operation_tasks_app
ON operation_tasks(app_id, created_at);

CREATE INDEX IF NOT EXISTS idx_operation_tasks_release
ON operation_tasks(release_id, created_at);

CREATE TRIGGER IF NOT EXISTS trg_operation_tasks_reject_active_deploy_app
BEFORE INSERT ON operation_tasks
WHEN NEW.app_id IS NOT NULL
  AND NEW.status IN ('queued', 'running')
  AND NEW.task_kind IN (
    'compose.up',
    'compose.down',
    'compose.restart',
    'binary.restart',
    'binary.stop',
    'artifact.git_publish',
    'release.deploy',
    'release.rollback',
    'release.manual_apply'
  )
  AND EXISTS (
    SELECT 1
    FROM operation_tasks existing
    WHERE existing.app_id = NEW.app_id
      AND existing.status IN ('queued', 'running')
      AND existing.task_kind IN (
        'compose.up',
        'compose.down',
        'compose.restart',
        'binary.restart',
        'binary.stop',
        'artifact.git_publish',
        'release.deploy',
        'release.rollback',
        'release.manual_apply'
      )
  )
BEGIN
  SELECT RAISE(ABORT, 'active deployment task exists for app');
END;

PRAGMA foreign_keys = ON;
