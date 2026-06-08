UPDATE admin_permissions
SET permission_key = 'apps.status',
    permission_name = '启停应用',
    description = '停用或重新启用部署应用。',
    resource_key = 'apps.status',
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE permission_key = 'apps.delete'
  AND NOT EXISTS (
      SELECT 1
      FROM admin_permissions
      WHERE permission_key = 'apps.status'
  );

UPDATE admin_permissions
SET permission_name = '启停应用',
    description = '停用或重新启用部署应用。',
    resource_type = 'action',
    resource_key = 'apps.status',
    module = '应用',
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE permission_key = 'apps.status';

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT rp.role_id, status_permission.id
FROM admin_role_permissions rp
JOIN admin_permissions legacy_permission ON legacy_permission.id = rp.permission_id
JOIN admin_permissions status_permission ON status_permission.permission_key = 'apps.status'
WHERE legacy_permission.permission_key = 'apps.delete';

DELETE FROM admin_role_permissions
WHERE permission_id IN (
    SELECT id
    FROM admin_permissions
    WHERE permission_key = 'apps.delete'
);

DELETE FROM admin_permissions
WHERE permission_key = 'apps.delete';

INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('apps.status', '启停应用', '停用或重新启用部署应用。', 'action', 'apps.status', '应用'),
    ('tasks.retry', '重试任务', '从失败的部署任务重新发起一次任务。', 'action', 'tasks.retry', '部署任务'),
    ('artifacts.upload', '上传制品', '上传二进制制品并登记新版本。', 'action', 'artifacts.upload', '制品')
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
JOIN admin_permissions p ON p.permission_key = 'tasks.retry'
WHERE r.role_code IN ('super_admin', 'admin', 'deployer');

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p ON p.permission_key = 'artifacts.upload'
WHERE r.role_code IN ('super_admin', 'admin', 'operator');

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p ON p.permission_key = 'apps.status'
WHERE r.role_code IN ('super_admin', 'admin', 'operator');
