CREATE TABLE IF NOT EXISTS operation_tasks (
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
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_operation_tasks_status ON operation_tasks(status, created_at);
CREATE INDEX IF NOT EXISTS idx_operation_tasks_app ON operation_tasks(app_id, created_at);

CREATE TABLE IF NOT EXISTS operation_task_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES operation_tasks(id) ON DELETE CASCADE,
    stream TEXT NOT NULL DEFAULT 'system' CHECK (stream IN ('system', 'stdout', 'stderr', 'combined')),
    content TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_operation_task_logs_task ON operation_task_logs(task_id, id);

CREATE TABLE IF NOT EXISTS deployment_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    task_id INTEGER REFERENCES operation_tasks(id) ON DELETE SET NULL,
    deploy_action TEXT NOT NULL CHECK (deploy_action IN ('compose_up', 'compose_down', 'compose_restart')),
    status TEXT NOT NULL CHECK (status IN ('running', 'success', 'failed')),
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    finished_at TEXT,
    message TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_deployment_runs_app ON deployment_runs(app_id, created_at);
