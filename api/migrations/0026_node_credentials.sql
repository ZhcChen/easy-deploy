CREATE TABLE IF NOT EXISTS node_credentials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    credential_key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    credential_type TEXT NOT NULL DEFAULT 'ssh_key' CHECK (credential_type IN ('ssh_key')),
    public_key TEXT NOT NULL DEFAULT '',
    private_key_path TEXT NOT NULL DEFAULT '',
    fingerprint TEXT NOT NULL DEFAULT '',
    passphrase_hint TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    created_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_node_credentials_status ON node_credentials(status);

ALTER TABLE nodes
ADD COLUMN credential_id INTEGER REFERENCES node_credentials(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_nodes_credential_id ON nodes(credential_id);

INSERT INTO admin_permissions(
    permission_key,
    permission_name,
    description,
    resource_type,
    resource_key,
    module
)
VALUES
    (
        'node_credentials.view',
        '查看节点凭据',
        '查看用于 SSH 节点免密登录的公钥、指纹和绑定关系。',
        'page',
        'node_credentials',
        '节点'
    ),
    (
        'node_credentials.manage',
        '管理节点凭据',
        '生成或录入 SSH 密钥，并维护节点凭据状态。',
        'action',
        'node_credentials.manage',
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
  AND p.permission_key IN ('node_credentials.view', 'node_credentials.manage');

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p
WHERE r.role_code IN ('deployer', 'viewer', 'auditor')
  AND p.permission_key = 'node_credentials.view';
