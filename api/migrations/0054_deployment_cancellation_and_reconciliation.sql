ALTER TABLE environment_deployment_runs
ADD COLUMN cancel_requested_at TEXT;

ALTER TABLE environment_deployment_runs
ADD COLUMN cancel_requested_by TEXT NOT NULL DEFAULT '';

ALTER TABLE environment_deployment_runs
ADD COLUMN reconciled_at TEXT;

ALTER TABLE environment_deployment_runs
ADD COLUMN reconciled_by TEXT NOT NULL DEFAULT '';

ALTER TABLE environment_deployment_runs
ADD COLUMN reconciliation_note TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_environment_deployment_cancel_requested
ON environment_deployment_runs(status, cancel_requested_at)
WHERE cancel_requested_at IS NOT NULL;
