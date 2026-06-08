CREATE TABLE IF NOT EXISTS platform_settings (
    setting_key TEXT PRIMARY KEY,
    setting_value TEXT NOT NULL,
    updated_by TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT OR IGNORE INTO platform_settings(setting_key, setting_value, updated_by)
VALUES
    ('default_app_work_dir', '/opt/easy-deploy/apps/{app_key}', 'system'),
    ('default_node_work_dir', '/opt/easy-deploy/apps', 'system'),
    ('uploaded_binary_releases_to_keep', '4', 'system');

INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('settings.update', '修改平台设置', '修改部署默认值和平台可持久化配置。', 'action', 'settings.update', '设置')
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
JOIN admin_permissions p ON p.permission_key = 'settings.update'
WHERE r.role_code IN ('super_admin', 'admin');
