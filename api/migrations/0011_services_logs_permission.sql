INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('services.logs', '查看服务日志', '查看 Compose 或 systemd 服务最近日志。', 'action', 'services.logs', '服务')
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
JOIN admin_permissions p ON p.permission_key = 'services.logs'
WHERE r.role_code IN ('super_admin', 'admin', 'deployer', 'operator');
