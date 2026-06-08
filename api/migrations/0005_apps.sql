CREATE TABLE IF NOT EXISTS apps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    app_type TEXT NOT NULL CHECK (app_type IN ('compose', 'binary')),
    deploy_mode TEXT NOT NULL CHECK (deploy_mode IN ('compose', 'binary')),
    work_dir TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'ready', 'deploying', 'running', 'failed', 'disabled')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_apps_status ON apps(status);
CREATE INDEX IF NOT EXISTS idx_apps_type ON apps(app_type);

CREATE TABLE IF NOT EXISTS app_targets (
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
    target_role TEXT NOT NULL DEFAULT 'primary',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (app_id, node_id)
);

CREATE INDEX IF NOT EXISTS idx_app_targets_node ON app_targets(node_id);

CREATE TABLE IF NOT EXISTS app_runtime_states (
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    runtime_status TEXT NOT NULL DEFAULT 'unknown' CHECK (runtime_status IN ('unknown', 'healthy', 'unhealthy', 'deploying', 'stopped')),
    active_version TEXT NOT NULL DEFAULT '',
    service_count INTEGER NOT NULL DEFAULT 0,
    message TEXT NOT NULL DEFAULT '',
    last_deploy_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (app_id, node_id)
);

CREATE TABLE IF NOT EXISTS app_config_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id INTEGER NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    snapshot_kind TEXT NOT NULL CHECK (snapshot_kind IN ('initial', 'deploy', 'manual')),
    compose_content TEXT NOT NULL DEFAULT '',
    env_content TEXT NOT NULL DEFAULT '',
    metadata TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_app_config_snapshots_app ON app_config_snapshots(app_id, created_at);
