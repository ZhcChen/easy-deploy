CREATE TABLE IF NOT EXISTS operation_task_phases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES operation_tasks(id) ON DELETE CASCADE,
    phase_no INTEGER NOT NULL,
    phase_key TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'running', 'success', 'failed', 'skipped')),
    summary TEXT NOT NULL DEFAULT '',
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(task_id, phase_no),
    UNIQUE(task_id, phase_key)
);

CREATE INDEX IF NOT EXISTS idx_operation_task_phases_task
ON operation_task_phases(task_id, phase_no);

ALTER TABLE operation_task_steps
ADD COLUMN phase_id INTEGER REFERENCES operation_task_phases(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_operation_task_steps_phase
ON operation_task_steps(phase_id, step_no, id);
