ALTER TABLE binary_artifacts
ADD COLUMN version_code INTEGER NOT NULL DEFAULT 0;

ALTER TABLE binary_artifacts
ADD COLUMN published_at TEXT NOT NULL DEFAULT '';

UPDATE binary_artifacts
SET
    version_code =
        CASE
            WHEN version GLOB 'v[0-9]*.[0-9]*.[0-9]*'
            THEN
                CAST(substr(version, 2, instr(substr(version, 2), '.') - 1) AS INTEGER) * 1000000
                + CAST(substr(
                    substr(version, instr(version, '.') + 1),
                    1,
                    instr(substr(version, instr(version, '.') + 1), '.') - 1
                ) AS INTEGER) * 1000
                + CAST(substr(
                    substr(version, instr(version, '.') + 1),
                    instr(substr(version, instr(version, '.') + 1), '.') + 1
                ) AS INTEGER)
            ELSE id
        END,
    published_at = created_at
WHERE version_code = 0
   OR published_at = '';

CREATE INDEX IF NOT EXISTS idx_binary_artifacts_app_version_code
ON binary_artifacts(app_id, version_code DESC, published_at DESC, id DESC);

UPDATE apps
SET status = 'ready'
WHERE status = 'draft';
