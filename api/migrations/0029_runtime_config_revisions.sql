ALTER TABLE app_config_snapshots
ADD COLUMN revision_no INTEGER NOT NULL DEFAULT 0;

ALTER TABLE app_config_snapshots
ADD COLUMN artifact_version TEXT NOT NULL DEFAULT '';

ALTER TABLE app_config_snapshots
ADD COLUMN config_hash TEXT NOT NULL DEFAULT '';

WITH ordered AS (
    SELECT
        id,
        ROW_NUMBER() OVER (PARTITION BY app_id ORDER BY id) AS revision_no
    FROM app_config_snapshots
)
UPDATE app_config_snapshots
SET revision_no = (
    SELECT ordered.revision_no
    FROM ordered
    WHERE ordered.id = app_config_snapshots.id
)
WHERE revision_no = 0;

CREATE UNIQUE INDEX IF NOT EXISTS idx_app_config_snapshots_revision
ON app_config_snapshots(app_id, revision_no);

ALTER TABLE deployment_runs
ADD COLUMN config_snapshot_id INTEGER REFERENCES app_config_snapshots(id) ON DELETE SET NULL;

ALTER TABLE deployment_runs
ADD COLUMN config_revision_no INTEGER NOT NULL DEFAULT 0;

ALTER TABLE deployment_runs
ADD COLUMN artifact_version TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_deployment_runs_config_snapshot
ON deployment_runs(config_snapshot_id);
