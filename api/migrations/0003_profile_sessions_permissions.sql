INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('profile.view', '查看个人中心', '查看当前登录账号信息和修改密码。', 'page', 'profile', '个人'),
    ('profile.password.change', '修改个人密码', '修改当前登录账号密码。', 'action', 'profile.password.change', '个人'),
    ('rbac.sessions.view', '查看会话', '查看后台登录会话。', 'page', 'rbac.sessions', '权限'),
    ('rbac.sessions.manage', '管理会话', '强制下线后台登录会话。', 'action', 'rbac.sessions.manage', '权限')
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
WHERE r.role_code IN ('super_admin', 'admin')
  AND p.permission_key IN (
      'profile.view',
      'profile.password.change',
      'rbac.sessions.view',
      'rbac.sessions.manage'
  );

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p
WHERE r.role_code IN ('deployer', 'operator', 'viewer', 'auditor')
  AND p.permission_key IN ('profile.view', 'profile.password.change');
