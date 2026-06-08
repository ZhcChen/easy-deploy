CREATE TABLE IF NOT EXISTS nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    node_type TEXT NOT NULL CHECK (node_type IN ('local', 'ssh')),
    address TEXT NOT NULL,
    ssh_port INTEGER NOT NULL DEFAULT 22,
    ssh_user TEXT NOT NULL DEFAULT '',
    work_dir TEXT NOT NULL DEFAULT '',
    region TEXT NOT NULL DEFAULT '',
    labels TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'unknown' CHECK (status IN ('online', 'offline', 'unknown', 'disabled')),
    docker_status TEXT NOT NULL DEFAULT 'unknown',
    last_check_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_nodes_type_status ON nodes(node_type, status);
CREATE INDEX IF NOT EXISTS idx_nodes_region ON nodes(region);

CREATE TABLE IF NOT EXISTS node_checks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    check_status TEXT NOT NULL CHECK (check_status IN ('passed', 'failed')),
    message TEXT NOT NULL DEFAULT '',
    docker_version TEXT NOT NULL DEFAULT '',
    compose_version TEXT NOT NULL DEFAULT '',
    checked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_node_checks_node_time ON node_checks(node_id, checked_at);

INSERT INTO nodes(
    node_key,
    name,
    node_type,
    address,
    ssh_port,
    ssh_user,
    work_dir,
    region,
    labels,
    status,
    docker_status,
    last_check_at
)
VALUES (
    'local',
    '本机节点',
    'local',
    '127.0.0.1',
    22,
    '',
    '.easy-deploy/apps',
    'local',
    'local,docker',
    'unknown',
    'unknown',
    NULL
)
ON CONFLICT(node_key) DO NOTHING;

INSERT INTO admin_permissions(
    permission_key,
    permission_name,
    description,
    resource_type,
    resource_key,
    module
)
VALUES (
    'nodes.manage',
    '管理节点',
    '新增、编辑和探测部署目标机器。',
    'action',
    'nodes.manage',
    '节点'
)
ON CONFLICT(permission_key) DO UPDATE SET
    permission_name = excluded.permission_name,
    description = excluded.description,
    resource_type = excluded.resource_type,
    resource_key = excluded.resource_key,
    module = excluded.module,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p
WHERE r.role_code IN ('super_admin', 'admin', 'operator')
  AND p.permission_key = 'nodes.manage';
