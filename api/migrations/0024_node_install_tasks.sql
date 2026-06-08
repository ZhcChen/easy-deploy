INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('nodes.install', '安装节点组件', '在节点上安装 Docker、Compose、Caddy 或 Nginx 等组件。', 'action', 'nodes.install', '节点')
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
JOIN admin_permissions p ON p.permission_key = 'nodes.install'
WHERE r.role_code IN ('super_admin', 'admin', 'operator');
