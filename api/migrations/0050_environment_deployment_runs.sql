-- Add migration script here.
ALTER TABLE app_release_queue
ADD COLUMN environment_id INTEGER REFERENCES app_environments(id) ON DELETE CASCADE;

ALTER TABLE app_release_queue
ADD COLUMN deployment_mode TEXT NOT NULL DEFAULT 'normal'
CHECK (deployment_mode IN ('normal', 'force'));

UPDATE app_release_queue
SET environment_id = (
    SELECT environments.id
    FROM app_environments environments
    WHERE environments.app_id = app_release_queue.app_id
    ORDER BY environments.id
    LIMIT 1
)
WHERE environment_id IS NULL;

DROP INDEX IF EXISTS idx_app_release_queue_single_running_app;

CREATE UNIQUE INDEX IF NOT EXISTS idx_app_release_queue_single_running_environment
ON app_release_queue(environment_id)
WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_app_release_queue_environment_seq
ON app_release_queue(environment_id, queue_seq, id);

ALTER TABLE operation_tasks
ADD COLUMN environment_id INTEGER REFERENCES app_environments(id) ON DELETE SET NULL;

UPDATE operation_tasks
SET environment_id = (
    SELECT environments.id
    FROM app_environments environments
    WHERE environments.app_id = operation_tasks.app_id
    ORDER BY environments.id
    LIMIT 1
)
WHERE app_id IS NOT NULL AND environment_id IS NULL;

CREATE INDEX IF NOT EXISTS idx_operation_tasks_environment
ON operation_tasks(environment_id, created_at);

ALTER TABLE deployment_runs
ADD COLUMN environment_id INTEGER REFERENCES app_environments(id) ON DELETE SET NULL;

ALTER TABLE deployment_runs
ADD COLUMN deployment_mode TEXT NOT NULL DEFAULT 'normal'
CHECK (deployment_mode IN ('normal', 'force'));

UPDATE deployment_runs
SET environment_id = (
    SELECT environments.id
    FROM app_environments environments
    WHERE environments.app_id = deployment_runs.app_id
    ORDER BY environments.id
    LIMIT 1
)
WHERE environment_id IS NULL;

CREATE INDEX IF NOT EXISTS idx_deployment_runs_environment
ON deployment_runs(environment_id, created_at);

CREATE TABLE IF NOT EXISTS environment_deployment_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    environment_id INTEGER NOT NULL REFERENCES app_environments(id) ON DELETE CASCADE,
    app_release_id INTEGER NOT NULL REFERENCES app_releases(id) ON DELETE RESTRICT,
    config_revision_id INTEGER NOT NULL REFERENCES app_config_revisions(id) ON DELETE RESTRICT,
    task_id INTEGER REFERENCES operation_tasks(id) ON DELETE SET NULL,
    deployment_mode TEXT NOT NULL CHECK (deployment_mode IN ('normal', 'force')),
    plan_hash TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'reconciling', 'success', 'partial_failed', 'all_failed', 'canceled')),
    summary TEXT NOT NULL DEFAULT '',
    created_by TEXT NOT NULL DEFAULT '',
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_environment_deployment_active
ON environment_deployment_runs(environment_id)
WHERE status IN ('queued', 'running', 'reconciling');

CREATE INDEX IF NOT EXISTS idx_environment_deployment_history
ON environment_deployment_runs(environment_id, created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS deployment_unit_run_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    deployment_run_id INTEGER NOT NULL REFERENCES environment_deployment_runs(id) ON DELETE CASCADE,
    unit_id INTEGER NOT NULL REFERENCES deployment_units(id) ON DELETE RESTRICT,
    unit_release_id INTEGER REFERENCES deployment_unit_releases(id) ON DELETE RESTRICT,
    stage_no INTEGER NOT NULL CHECK (stage_no >= 0),
    action TEXT NOT NULL CHECK (action IN ('deploy', 'skip', 'start', 'stop', 'upgrade', 'downgrade', 'restore', 'application_check')),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'success', 'failed', 'skipped', 'not_started', 'canceled_unknown')),
    target_fingerprint TEXT NOT NULL DEFAULT '',
    previous_fingerprint TEXT NOT NULL DEFAULT '',
    failure_kind TEXT NOT NULL DEFAULT '',
    failure_summary TEXT NOT NULL DEFAULT '',
    exit_code INTEGER,
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(deployment_run_id, unit_id)
);

CREATE INDEX IF NOT EXISTS idx_deployment_unit_results_run
ON deployment_unit_run_results(deployment_run_id, stage_no, id);

CREATE TABLE IF NOT EXISTS deployment_unit_runtime_states (
    environment_id INTEGER NOT NULL REFERENCES app_environments(id) ON DELETE CASCADE,
    unit_id INTEGER NOT NULL REFERENCES deployment_units(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    runtime_status TEXT NOT NULL DEFAULT 'unknown'
        CHECK (runtime_status IN ('unknown', 'healthy', 'unhealthy', 'deploying', 'stopped')),
    active_unit_release_id INTEGER REFERENCES deployment_unit_releases(id) ON DELETE SET NULL,
    active_fingerprint TEXT NOT NULL DEFAULT '',
    container_version_label TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    last_deployment_run_id INTEGER REFERENCES environment_deployment_runs(id) ON DELETE SET NULL,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (environment_id, unit_id, node_id)
);

DROP TRIGGER IF EXISTS trg_operation_tasks_reject_active_deploy_app;

CREATE TRIGGER IF NOT EXISTS trg_operation_tasks_reject_active_deploy_environment
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
    WHERE existing.status IN ('queued', 'running')
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
      AND (
        (NEW.environment_id IS NOT NULL AND existing.environment_id = NEW.environment_id)
        OR (NEW.environment_id IS NULL AND existing.environment_id IS NULL AND existing.app_id = NEW.app_id)
      )
  )
BEGIN
  SELECT RAISE(ABORT, 'active deployment task exists for app');
END;
