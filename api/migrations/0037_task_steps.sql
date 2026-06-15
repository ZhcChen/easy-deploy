CREATE TABLE IF NOT EXISTS operation_task_steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES operation_tasks(id) ON DELETE CASCADE,
    node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
    step_no INTEGER NOT NULL,
    step_key TEXT NOT NULL,
    title TEXT NOT NULL,
    command TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'success', 'failed', 'skipped')),
    exit_code INTEGER,
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(task_id, step_no)
);

CREATE INDEX IF NOT EXISTS idx_operation_task_steps_task
ON operation_task_steps(task_id, step_no);

CREATE INDEX IF NOT EXISTS idx_operation_task_steps_node
ON operation_task_steps(node_id, task_id);

ALTER TABLE operation_task_logs
ADD COLUMN step_id INTEGER REFERENCES operation_task_steps(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_operation_task_logs_step
ON operation_task_logs(step_id, id);
