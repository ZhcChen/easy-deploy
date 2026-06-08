CREATE TABLE IF NOT EXISTS binary_artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    artifact_path TEXT NOT NULL,
    artifact_kind TEXT NOT NULL DEFAULT 'binary' CHECK (artifact_kind IN ('binary', 'tar_gz')),
    status TEXT NOT NULL DEFAULT 'registered' CHECK (status IN ('registered', 'active', 'disabled')),
    metadata TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id, version)
);

CREATE INDEX IF NOT EXISTS idx_binary_artifacts_app ON binary_artifacts(app_id, created_at);

CREATE TABLE IF NOT EXISTS app_binary_configs (
    app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
    service_name TEXT NOT NULL DEFAULT '',
    artifact_version TEXT NOT NULL DEFAULT '',
    artifact_path TEXT NOT NULL DEFAULT '',
    exec_args TEXT NOT NULL DEFAULT '',
    working_dir TEXT NOT NULL DEFAULT '',
    service_user TEXT NOT NULL DEFAULT 'deploy',
    unit_name TEXT NOT NULL DEFAULT '',
    env_content TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

PRAGMA foreign_keys = OFF;

ALTER TABLE deployment_runs RENAME TO deployment_runs_old;

CREATE TABLE deployment_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    task_id INTEGER REFERENCES operation_tasks(id) ON DELETE SET NULL,
    deploy_action TEXT NOT NULL CHECK (deploy_action IN ('compose_up', 'compose_down', 'compose_restart', 'binary_restart', 'binary_stop')),
    status TEXT NOT NULL CHECK (status IN ('running', 'success', 'failed')),
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT,
    message TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO deployment_runs(
    id,
    app_id,
    task_id,
    deploy_action,
    status,
    started_at,
    finished_at,
    message,
    created_at
)
SELECT
    id,
    app_id,
    task_id,
    deploy_action,
    status,
    started_at,
    finished_at,
    message,
    created_at
FROM deployment_runs_old;

DROP TABLE deployment_runs_old;

CREATE INDEX IF NOT EXISTS idx_deployment_runs_app ON deployment_runs(app_id, created_at);

ALTER TABLE app_health_checks RENAME TO app_health_checks_old;

CREATE TABLE app_health_checks (
    app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
    check_kind TEXT NOT NULL DEFAULT 'none' CHECK (check_kind IN ('none', 'http', 'tcp', 'compose_running', 'systemd_active')),
    endpoint TEXT NOT NULL DEFAULT '',
    timeout_secs INTEGER NOT NULL DEFAULT 5,
    expected_status INTEGER NOT NULL DEFAULT 200,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO app_health_checks(
    app_id,
    check_kind,
    endpoint,
    timeout_secs,
    expected_status,
    updated_at
)
SELECT
    app_id,
    check_kind,
    endpoint,
    timeout_secs,
    expected_status,
    updated_at
FROM app_health_checks_old;

DROP TABLE app_health_checks_old;

PRAGMA foreign_keys = ON;
