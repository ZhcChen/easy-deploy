-- Add migration script here.
CREATE TABLE IF NOT EXISTS deployment_unit_releases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_id INTEGER NOT NULL REFERENCES deployment_units(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    version_code INTEGER NOT NULL,
    package_name TEXT NOT NULL,
    package_path TEXT NOT NULL DEFAULT '',
    extract_dir TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'openapi' CHECK (source IN ('openapi', 'web', 'migration')),
    checksum_sha256 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    published_at TEXT NOT NULL DEFAULT '',
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    metadata TEXT NOT NULL DEFAULT '{}',
    storage_provider TEXT NOT NULL DEFAULT 'local' CHECK (storage_provider IN ('local', 'aliyun_oss')),
    storage_bucket TEXT NOT NULL DEFAULT '',
    storage_object_key TEXT NOT NULL DEFAULT '',
    storage_endpoint TEXT NOT NULL DEFAULT '',
    storage_object_version_id TEXT NOT NULL DEFAULT '',
    storage_integrity TEXT NOT NULL DEFAULT 'local'
        CHECK (storage_integrity IN ('legacy', 'local', 'unique_key', 'version_pinned')),
    artifact_status TEXT NOT NULL DEFAULT 'active'
        CHECK (artifact_status IN ('active', 'deleting', 'delete_failed', 'deleted')),
    cleanup_error TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(unit_id, version),
    UNIQUE(unit_id, version_code)
);

CREATE INDEX IF NOT EXISTS idx_unit_releases_unit_code
ON deployment_unit_releases(unit_id, version_code DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_unit_releases_storage
ON deployment_unit_releases(storage_provider, storage_bucket, storage_object_key);

CREATE TABLE IF NOT EXISTS application_release_manifests (
    app_release_id INTEGER PRIMARY KEY REFERENCES app_releases(id) ON DELETE CASCADE,
    base_app_release_id INTEGER REFERENCES app_releases(id) ON DELETE RESTRICT,
    manifest_hash TEXT NOT NULL,
    manifest_json TEXT NOT NULL,
    immutable_status TEXT NOT NULL DEFAULT 'ready'
        CHECK (immutable_status IN ('ready', 'archived', 'deleting', 'deleted')),
    created_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    archived_at TEXT,
    UNIQUE(manifest_hash)
);

CREATE INDEX IF NOT EXISTS idx_application_release_base
ON application_release_manifests(base_app_release_id);

CREATE TABLE IF NOT EXISTS app_release_units (
    app_release_id INTEGER NOT NULL REFERENCES app_releases(id) ON DELETE CASCADE,
    unit_id INTEGER NOT NULL REFERENCES deployment_units(id) ON DELETE RESTRICT,
    unit_release_id INTEGER REFERENCES deployment_unit_releases(id) ON DELETE RESTRICT,
    desired_status TEXT NOT NULL DEFAULT 'active' CHECK (desired_status IN ('active', 'disabled')),
    stage_no INTEGER NOT NULL DEFAULT 1 CHECK (stage_no >= 1),
    unit_order INTEGER NOT NULL DEFAULT 1 CHECK (unit_order >= 1),
    removal_order INTEGER NOT NULL DEFAULT 1 CHECK (removal_order >= 1),
    target_fingerprint TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (app_release_id, unit_id)
);

CREATE INDEX IF NOT EXISTS idx_app_release_units_release_order
ON app_release_units(app_release_id, stage_no, unit_order, unit_id);

CREATE INDEX IF NOT EXISTS idx_app_release_units_unit_release
ON app_release_units(unit_release_id, app_release_id);

CREATE TABLE IF NOT EXISTS app_release_environment_configs (
    app_release_id INTEGER NOT NULL REFERENCES app_releases(id) ON DELETE CASCADE,
    environment_id INTEGER NOT NULL REFERENCES app_environments(id) ON DELETE RESTRICT,
    config_revision_id INTEGER NOT NULL REFERENCES app_config_revisions(id) ON DELETE RESTRICT,
    PRIMARY KEY (app_release_id, environment_id)
);

CREATE INDEX IF NOT EXISTS idx_release_environment_config_revision
ON app_release_environment_configs(config_revision_id, app_release_id);

CREATE TABLE IF NOT EXISTS api_idempotency_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token_id INTEGER NOT NULL REFERENCES api_tokens(id) ON DELETE CASCADE,
    action TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    response_status INTEGER NOT NULL,
    response_body TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(token_id, action, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_api_idempotency_expires
ON api_idempotency_records(expires_at);

INSERT INTO deployment_unit_releases(
    unit_id,
    version,
    version_code,
    package_name,
    package_path,
    extract_dir,
    source,
    checksum_sha256,
    size_bytes,
    published_at,
    received_at,
    metadata,
    storage_provider,
    storage_bucket,
    storage_object_key,
    storage_endpoint,
    storage_object_version_id,
    storage_integrity,
    created_at,
    updated_at
)
SELECT
    units.id,
    releases.version,
    releases.version_code,
    releases.package_name,
    releases.package_path,
    releases.extract_dir,
    'migration',
    releases.checksum_sha256,
    releases.size_bytes,
    releases.published_at,
    releases.received_at,
    releases.metadata,
    releases.storage_provider,
    releases.storage_bucket,
    releases.storage_object_key,
    releases.storage_endpoint,
    releases.storage_object_version_id,
    releases.storage_integrity,
    releases.created_at,
    releases.updated_at
FROM app_releases releases
JOIN deployment_units units ON units.app_id = releases.app_id AND units.unit_key = 'default'
WHERE trim(releases.package_name) <> ''
  AND NOT EXISTS (
      SELECT 1
      FROM deployment_unit_releases existing
      WHERE existing.unit_id = units.id AND existing.version = releases.version
  );

INSERT INTO application_release_manifests(
    app_release_id,
    manifest_hash,
    manifest_json,
    immutable_status,
    created_by,
    created_at
)
SELECT
    releases.id,
    'legacy-release-' || releases.id || '-' || releases.checksum_sha256,
    json_object('legacy_release_id', releases.id, 'version', releases.version, 'unit_key', 'default'),
    'ready',
    'migration-0049',
    releases.created_at
FROM app_releases releases
WHERE NOT EXISTS (
    SELECT 1 FROM application_release_manifests manifests WHERE manifests.app_release_id = releases.id
);

INSERT INTO app_release_units(
    app_release_id,
    unit_id,
    unit_release_id,
    desired_status,
    stage_no,
    unit_order,
    removal_order,
    target_fingerprint
)
SELECT
    releases.id,
    units.id,
    unit_releases.id,
    CASE WHEN units.lifecycle_status = 'disabled' THEN 'disabled' ELSE 'active' END,
    1,
    1,
    1,
    releases.checksum_sha256 || ':' || COALESCE(snapshots.config_hash, '')
FROM app_releases releases
JOIN deployment_units units ON units.app_id = releases.app_id AND units.unit_key = 'default'
LEFT JOIN deployment_unit_releases unit_releases
    ON unit_releases.unit_id = units.id AND unit_releases.version = releases.version
LEFT JOIN app_config_snapshots snapshots ON snapshots.id = (
    SELECT candidate.id
    FROM app_config_snapshots candidate
    WHERE candidate.app_id = releases.app_id
    ORDER BY candidate.revision_no DESC, candidate.id DESC
    LIMIT 1
)
WHERE NOT EXISTS (
    SELECT 1
    FROM app_release_units existing
    WHERE existing.app_release_id = releases.id AND existing.unit_id = units.id
);

INSERT INTO app_release_environment_configs(app_release_id, environment_id, config_revision_id)
SELECT releases.id, environments.id, revisions.id
FROM app_releases releases
JOIN app_environments environments ON environments.app_id = releases.app_id
JOIN app_config_revisions revisions ON revisions.id = environments.current_config_revision_id
WHERE NOT EXISTS (
    SELECT 1
    FROM app_release_environment_configs existing
    WHERE existing.app_release_id = releases.id AND existing.environment_id = environments.id
);

UPDATE app_environments
SET current_app_release_id = (
    SELECT releases.id
    FROM app_releases releases
    WHERE releases.app_id = app_environments.app_id
      AND releases.status = 'deployed'
    ORDER BY releases.version_code DESC, releases.id DESC
    LIMIT 1
)
WHERE current_app_release_id IS NULL;

INSERT INTO version_counters(scope_kind, scope_id, next_value)
SELECT 'unit_release', units.id, MAX(100, COALESCE(MAX(releases.version_code) + 1, 100))
FROM deployment_units units
LEFT JOIN deployment_unit_releases releases ON releases.unit_id = units.id
WHERE true
GROUP BY units.id
ON CONFLICT(scope_kind, scope_id) DO UPDATE SET next_value = MAX(version_counters.next_value, excluded.next_value);

INSERT INTO version_counters(scope_kind, scope_id, next_value)
SELECT 'app_release', apps.id, MAX(100, COALESCE(MAX(releases.version_code) + 1, 100))
FROM apps
LEFT JOIN app_releases releases ON releases.app_id = apps.id
WHERE true
GROUP BY apps.id
ON CONFLICT(scope_kind, scope_id) DO UPDATE SET next_value = MAX(version_counters.next_value, excluded.next_value);
