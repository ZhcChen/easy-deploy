ALTER TABLE apps
ADD COLUMN release_source TEXT NOT NULL DEFAULT 'manual'
CHECK (release_source IN ('manual', 'package_upload'));

ALTER TABLE apps
ADD COLUMN compose_strategy TEXT NOT NULL DEFAULT 'recreate'
CHECK (compose_strategy IN ('recreate', 'blue_green'));

CREATE INDEX IF NOT EXISTS idx_apps_release_source
ON apps(release_source);

CREATE INDEX IF NOT EXISTS idx_apps_compose_strategy
ON apps(compose_strategy);

CREATE TABLE IF NOT EXISTS app_releases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    version_code INTEGER NOT NULL DEFAULT 0,
    package_name TEXT NOT NULL DEFAULT '',
    package_path TEXT NOT NULL DEFAULT '',
    extract_dir TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'received'
        CHECK (status IN ('received', 'queued', 'deploying', 'deployed', 'failed', 'rolled_back', 'canceled')),
    source TEXT NOT NULL DEFAULT 'web'
        CHECK (source IN ('openapi', 'web')),
    checksum_sha256 TEXT NOT NULL DEFAULT '',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    published_at TEXT NOT NULL DEFAULT '',
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    metadata TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id, version)
);

CREATE INDEX IF NOT EXISTS idx_app_releases_app_version_code
ON app_releases(app_id, version_code DESC, published_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_app_releases_status_received
ON app_releases(status, received_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS app_release_queue (
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
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

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

ALTER TABLE operation_tasks
ADD COLUMN release_id INTEGER REFERENCES app_releases(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_operation_tasks_release
ON operation_tasks(release_id, created_at);

ALTER TABLE deployment_runs
ADD COLUMN release_id INTEGER REFERENCES app_releases(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_deployment_runs_release
ON deployment_runs(release_id, created_at);

DROP TRIGGER IF EXISTS trg_operation_tasks_reject_active_deploy_app;

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
