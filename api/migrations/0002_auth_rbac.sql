CREATE TABLE IF NOT EXISTS admin_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled', 'locked')),
    is_super_admin INTEGER NOT NULL DEFAULT 0 CHECK (is_super_admin IN (0, 1)),
    last_login_at TEXT,
    last_login_ip TEXT NOT NULL DEFAULT '',
    password_changed_at TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_admin_accounts_status ON admin_accounts(status);

CREATE TABLE IF NOT EXISTS admin_roles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    role_code TEXT NOT NULL UNIQUE,
    role_name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    is_system INTEGER NOT NULL DEFAULT 0 CHECK (is_system IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_admin_roles_status ON admin_roles(status);

CREATE TABLE IF NOT EXISTS admin_permissions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    permission_key TEXT NOT NULL UNIQUE,
    permission_name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    resource_type TEXT NOT NULL CHECK (resource_type IN ('page', 'action')),
    resource_key TEXT NOT NULL,
    module TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_admin_permissions_module ON admin_permissions(module);
CREATE INDEX IF NOT EXISTS idx_admin_permissions_resource ON admin_permissions(resource_type, resource_key);

CREATE TABLE IF NOT EXISTS admin_account_roles (
    account_id INTEGER NOT NULL REFERENCES admin_accounts(id) ON DELETE CASCADE,
    role_id INTEGER NOT NULL REFERENCES admin_roles(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (account_id, role_id)
);

CREATE INDEX IF NOT EXISTS idx_admin_account_roles_role_id ON admin_account_roles(role_id);

CREATE TABLE IF NOT EXISTS admin_role_permissions (
    role_id INTEGER NOT NULL REFERENCES admin_roles(id) ON DELETE CASCADE,
    permission_id INTEGER NOT NULL REFERENCES admin_permissions(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (role_id, permission_id)
);

CREATE INDEX IF NOT EXISTS idx_admin_role_permissions_permission_id ON admin_role_permissions(permission_id);

CREATE TABLE IF NOT EXISTS admin_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES admin_accounts(id) ON DELETE CASCADE,
    session_status TEXT NOT NULL DEFAULT 'active' CHECK (session_status IN ('active', 'revoked', 'expired')),
    access_token_hash TEXT NOT NULL UNIQUE,
    refresh_token_hash TEXT NOT NULL UNIQUE,
    access_expires_at TEXT NOT NULL,
    refresh_expires_at TEXT NOT NULL,
    last_ip TEXT NOT NULL DEFAULT '',
    user_agent TEXT NOT NULL DEFAULT '',
    revoked_at TEXT,
    revoke_reason TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_admin_sessions_account ON admin_sessions(account_id, session_status);
CREATE INDEX IF NOT EXISTS idx_admin_sessions_access_hash ON admin_sessions(access_token_hash);
CREATE INDEX IF NOT EXISTS idx_admin_sessions_refresh_hash ON admin_sessions(refresh_token_hash);

CREATE TABLE IF NOT EXISTS admin_audit_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_account_id INTEGER REFERENCES admin_accounts(id) ON DELETE SET NULL,
    actor_username TEXT NOT NULL DEFAULT '',
    action TEXT NOT NULL,
    target_type TEXT NOT NULL DEFAULT '',
    target_id TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    ip TEXT NOT NULL DEFAULT '',
    user_agent TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_admin_audit_logs_action ON admin_audit_logs(action, created_at);

INSERT INTO admin_roles(role_code, role_name, description, status, is_system)
VALUES
    ('super_admin', '超级管理员', '拥有全部权限，用于平台初始化和最高权限运维。', 'active', 1),
    ('admin', '平台管理员', '管理应用、服务、节点和后台账号。', 'active', 1),
    ('deployer', '部署人员', '执行部署、回滚并查看部署任务。', 'active', 1),
    ('operator', '运维人员', '维护应用、服务、节点和配置。', 'active', 1),
    ('viewer', '只读用户', '查看平台资源，不执行变更操作。', 'active', 1),
    ('auditor', '审计用户', '查看审计日志、部署记录和运行状态。', 'active', 1)
ON CONFLICT(role_code) DO UPDATE SET
    role_name = excluded.role_name,
    description = excluded.description,
    status = excluded.status,
    is_system = excluded.is_system,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');

INSERT INTO admin_permissions(permission_key, permission_name, description, resource_type, resource_key, module)
VALUES
    ('dashboard.view', '查看总览', '访问部署控制台总览页。', 'page', 'dashboard', '总览'),
    ('apps.view', '查看应用', '查看应用列表和详情。', 'page', 'apps', '应用'),
    ('apps.create', '创建应用', '创建新的部署应用。', 'action', 'apps.create', '应用'),
    ('apps.update', '编辑应用', '编辑应用基础信息和配置。', 'action', 'apps.update', '应用'),
    ('apps.delete', '删除应用', '删除部署应用。', 'action', 'apps.delete', '应用'),
    ('services.view', '查看服务', '查看服务列表、版本和运行状态。', 'page', 'services', '服务'),
    ('services.deploy', '部署服务', '发起服务部署任务。', 'action', 'services.deploy', '服务'),
    ('services.rollback', '回滚服务', '回滚服务到历史版本。', 'action', 'services.rollback', '服务'),
    ('nodes.view', '查看节点', '查看部署目标机器。', 'page', 'nodes', '节点'),
    ('nodes.create', '创建节点', '新增部署目标机器。', 'action', 'nodes.create', '节点'),
    ('nodes.update', '编辑节点', '修改节点连接和标签配置。', 'action', 'nodes.update', '节点'),
    ('nodes.delete', '删除节点', '删除部署目标机器。', 'action', 'nodes.delete', '节点'),
    ('tasks.view', '查看部署任务', '查看部署任务队列和历史记录。', 'page', 'tasks', '部署任务'),
    ('templates.view', '查看模板', '查看 Docker Compose 和二进制部署模板。', 'page', 'templates', '模板'),
    ('artifacts.view', '查看制品', '查看构建制品和版本。', 'page', 'artifacts', '制品'),
    ('rbac.accounts.view', '查看账号', '查看后台账号列表。', 'page', 'rbac.accounts', '权限'),
    ('rbac.accounts.manage', '管理账号', '创建、禁用、重置密码和分配角色。', 'action', 'rbac.accounts.manage', '权限'),
    ('rbac.roles.view', '查看角色', '查看角色和权限配置。', 'page', 'rbac.roles', '权限'),
    ('rbac.roles.manage', '管理角色', '创建角色并分配权限。', 'action', 'rbac.roles.manage', '权限'),
    ('settings.view', '查看设置', '查看平台设置。', 'page', 'settings', '设置'),
    ('audit.view', '查看审计日志', '查看账号和部署相关审计日志。', 'page', 'audit', '审计')
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
WHERE r.role_code IN ('super_admin', 'admin');

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p ON p.permission_key IN (
    'dashboard.view',
    'apps.view',
    'services.view',
    'services.deploy',
    'services.rollback',
    'nodes.view',
    'tasks.view',
    'templates.view',
    'artifacts.view'
)
WHERE r.role_code = 'deployer';

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p ON p.permission_key IN (
    'dashboard.view',
    'apps.view',
    'apps.create',
    'apps.update',
    'services.view',
    'nodes.view',
    'nodes.create',
    'nodes.update',
    'tasks.view',
    'templates.view',
    'artifacts.view',
    'settings.view'
)
WHERE r.role_code = 'operator';

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p ON p.permission_key IN (
    'dashboard.view',
    'apps.view',
    'services.view',
    'nodes.view',
    'tasks.view',
    'templates.view',
    'artifacts.view',
    'settings.view'
)
WHERE r.role_code = 'viewer';

INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id)
SELECT r.id, p.id
FROM admin_roles r
JOIN admin_permissions p ON p.permission_key IN (
    'dashboard.view',
    'apps.view',
    'services.view',
    'nodes.view',
    'tasks.view',
    'audit.view'
)
WHERE r.role_code = 'auditor';
