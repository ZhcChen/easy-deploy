CREATE TABLE IF NOT EXISTS operation_task_node_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES operation_tasks(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    node_name TEXT NOT NULL,
    node_key TEXT NOT NULL,
    node_type TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('success', 'failed', 'skipped')),
    message TEXT NOT NULL DEFAULT '',
    command_count INTEGER NOT NULL DEFAULT 0,
    started_at TEXT,
    finished_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(task_id, node_id)
);

CREATE INDEX IF NOT EXISTS idx_operation_task_node_results_task
ON operation_task_node_results(task_id, id);
