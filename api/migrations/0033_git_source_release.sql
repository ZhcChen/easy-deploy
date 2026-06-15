CREATE TABLE IF NOT EXISTS app_source_configs (
    app_id INTEGER PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
    source_type TEXT NOT NULL DEFAULT 'git' CHECK (source_type IN ('git')),
    repo_url TEXT NOT NULL DEFAULT '',
    credential_id INTEGER,
    tag_pattern TEXT NOT NULL DEFAULT 'v*',
    tag_prefix TEXT NOT NULL DEFAULT 'v',
    build_command TEXT NOT NULL DEFAULT '',
    artifact_path TEXT NOT NULL DEFAULT '',
    entry_file TEXT NOT NULL DEFAULT '',
    work_subdir TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'disabled' CHECK (status IN ('active', 'disabled')),
    last_tag TEXT NOT NULL DEFAULT '',
    last_commit TEXT NOT NULL DEFAULT '',
    last_task_id INTEGER REFERENCES operation_tasks(id) ON DELETE SET NULL,
    last_published_at TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_app_source_configs_status
ON app_source_configs(status);

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
    'artifact.git_publish'
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
        'artifact.git_publish'
      )
  )
BEGIN
  SELECT RAISE(ABORT, 'active deployment task exists for app');
END;
