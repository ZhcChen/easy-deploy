CREATE TABLE IF NOT EXISTS deployment_task_log_budgets (
    task_id INTEGER PRIMARY KEY REFERENCES operation_tasks(id) ON DELETE CASCADE,
    stored_bytes INTEGER NOT NULL DEFAULT 0 CHECK (stored_bytes >= 0),
    received_bytes INTEGER NOT NULL DEFAULT 0 CHECK (received_bytes >= 0),
    dropped_bytes INTEGER NOT NULL DEFAULT 0 CHECK (dropped_bytes >= 0),
    max_bytes INTEGER NOT NULL DEFAULT 104857600 CHECK (max_bytes > 0),
    truncated INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE IF NOT EXISTS deployment_step_log_buffers (
    step_id INTEGER PRIMARY KEY REFERENCES operation_task_steps(id) ON DELETE CASCADE,
    task_id INTEGER NOT NULL REFERENCES operation_tasks(id) ON DELETE CASCADE,
    head_content BLOB NOT NULL DEFAULT X'',
    tail_content BLOB NOT NULL DEFAULT X'',
    stored_bytes INTEGER NOT NULL DEFAULT 0 CHECK (stored_bytes >= 0),
    received_bytes INTEGER NOT NULL DEFAULT 0 CHECK (received_bytes >= 0),
    dropped_bytes INTEGER NOT NULL DEFAULT 0 CHECK (dropped_bytes >= 0),
    head_limit_bytes INTEGER NOT NULL DEFAULT 2097152 CHECK (head_limit_bytes >= 0),
    tail_limit_bytes INTEGER NOT NULL DEFAULT 8388608 CHECK (tail_limit_bytes >= 0),
    truncated INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
    finished INTEGER NOT NULL DEFAULT 0 CHECK (finished IN (0, 1)),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_deployment_step_logs_task
ON deployment_step_log_buffers(task_id, step_id);

ALTER TABLE environment_deployment_runs
ADD COLUMN snapshot_status TEXT NOT NULL DEFAULT 'active'
CHECK (snapshot_status IN ('active', 'deleted'));

ALTER TABLE environment_deployment_runs
ADD COLUMN snapshot_deleted_at TEXT;
