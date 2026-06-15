DROP TABLE IF EXISTS app_source_configs;

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
    'binary.stop'
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
        'binary.stop'
      )
  )
BEGIN
  SELECT RAISE(ABORT, 'active deployment task exists for app');
END;
