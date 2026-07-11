-- Add migration script here.
CREATE TABLE IF NOT EXISTS app_environments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    environment_key TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'configuring'
        CHECK (status IN ('configuring', 'ready', 'disabled')),
    max_parallel_units INTEGER NOT NULL DEFAULT 3
        CHECK (max_parallel_units BETWEEN 1 AND 32),
    current_app_release_id INTEGER REFERENCES app_releases(id) ON DELETE SET NULL,
    current_config_revision_id INTEGER REFERENCES app_config_revisions(id) ON DELETE SET NULL,
    runtime_status TEXT NOT NULL DEFAULT 'unknown'
        CHECK (runtime_status IN ('unknown', 'running', 'partial_unhealthy', 'stopped')),
    last_deployment_status TEXT NOT NULL DEFAULT 'waiting'
        CHECK (last_deployment_status IN ('waiting', 'running', 'success', 'partial_failed', 'all_failed', 'canceled')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id, environment_key)
);

CREATE INDEX IF NOT EXISTS idx_app_environments_app
ON app_environments(app_id, id);

CREATE TABLE IF NOT EXISTS app_environment_targets (
    environment_id INTEGER NOT NULL REFERENCES app_environments(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
    target_role TEXT NOT NULL DEFAULT 'primary',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (environment_id, node_id)
);

CREATE INDEX IF NOT EXISTS idx_app_environment_targets_node
ON app_environment_targets(node_id, environment_id);

CREATE TABLE IF NOT EXISTS deployment_units (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    unit_key TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    required INTEGER NOT NULL DEFAULT 1 CHECK (required IN (0, 1)),
    lifecycle_status TEXT NOT NULL DEFAULT 'active'
        CHECK (lifecycle_status IN ('active', 'disabled')),
    work_dir TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id, unit_key)
);

CREATE INDEX IF NOT EXISTS idx_deployment_units_app
ON deployment_units(app_id, lifecycle_status, id);

CREATE TABLE IF NOT EXISTS deployment_pipeline_stages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    stage_no INTEGER NOT NULL CHECK (stage_no >= 1),
    stage_key TEXT NOT NULL,
    name TEXT NOT NULL,
    stage_kind TEXT NOT NULL DEFAULT 'units'
        CHECK (stage_kind IN ('units', 'application_check')),
    check_config TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id, stage_no),
    UNIQUE(app_id, stage_key)
);

CREATE TABLE IF NOT EXISTS deployment_pipeline_stage_units (
    stage_id INTEGER NOT NULL REFERENCES deployment_pipeline_stages(id) ON DELETE CASCADE,
    unit_id INTEGER NOT NULL REFERENCES deployment_units(id) ON DELETE CASCADE,
    unit_order INTEGER NOT NULL DEFAULT 1 CHECK (unit_order >= 1),
    removal_order INTEGER NOT NULL DEFAULT 1 CHECK (removal_order >= 1),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (stage_id, unit_id)
);

CREATE INDEX IF NOT EXISTS idx_pipeline_stage_units_order
ON deployment_pipeline_stage_units(stage_id, unit_order, unit_id);

CREATE TABLE IF NOT EXISTS app_config_drafts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    draft_json TEXT NOT NULL DEFAULT '{}',
    draft_hash TEXT NOT NULL DEFAULT '',
    updated_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id)
);

CREATE TABLE IF NOT EXISTS app_config_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    revision_no INTEGER NOT NULL CHECK (revision_no >= 100),
    config_json TEXT NOT NULL,
    public_config_json TEXT NOT NULL,
    secret_ciphertext TEXT NOT NULL DEFAULT '',
    secret_fingerprints TEXT NOT NULL DEFAULT '{}',
    config_hash TEXT NOT NULL,
    script_hash TEXT NOT NULL DEFAULT '',
    encryption_key_id TEXT NOT NULL DEFAULT '',
    published_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(app_id, revision_no),
    UNIQUE(app_id, config_hash)
);

CREATE INDEX IF NOT EXISTS idx_app_config_revisions_app
ON app_config_revisions(app_id, revision_no DESC);

CREATE TABLE IF NOT EXISTS version_counters (
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('app_release', 'unit_release', 'config_revision')),
    scope_id INTEGER NOT NULL,
    next_value INTEGER NOT NULL DEFAULT 100 CHECK (next_value >= 100),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (scope_kind, scope_id)
);
