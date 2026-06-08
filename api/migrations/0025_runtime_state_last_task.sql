ALTER TABLE app_runtime_states
ADD COLUMN last_task_id INTEGER REFERENCES operation_tasks(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_app_runtime_states_last_task
ON app_runtime_states(last_task_id);
