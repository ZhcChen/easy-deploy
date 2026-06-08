INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('tasks.cancel', '取消任务', '取消尚未开始执行的排队任务。', 'action', 'tasks.cancel', '部署任务')
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
JOIN admin_permissions p ON p.permission_key = 'tasks.cancel'
WHERE r.role_code IN ('super_admin', 'admin', 'deployer');
