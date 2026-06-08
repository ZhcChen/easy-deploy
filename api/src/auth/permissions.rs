#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionResourceType {
    Page,
    Action,
}

impl PermissionResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Action => "action",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PermissionDef {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub resource_type: PermissionResourceType,
    pub resource_key: &'static str,
    pub module: &'static str,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PermissionView {
    pub id: i64,
    pub permission_key: String,
    pub permission_name: String,
    pub description: String,
    pub resource_type: String,
    pub module: String,
}

pub const DASHBOARD_VIEW: &str = "dashboard.view";
pub const APPS_VIEW: &str = "apps.view";
pub const APPS_STATUS: &str = "apps.status";
pub const SERVICES_VIEW: &str = "services.view";
pub const SERVICES_LOGS: &str = "services.logs";
pub const TASKS_RETRY: &str = "tasks.retry";
pub const NODES_VIEW: &str = "nodes.view";
pub const NODES_MANAGE: &str = "nodes.manage";
pub const NODES_INSTALL: &str = "nodes.install";
pub const NODE_CREDENTIALS_VIEW: &str = "node_credentials.view";
pub const NODE_CREDENTIALS_MANAGE: &str = "node_credentials.manage";
pub const TASKS_VIEW: &str = "tasks.view";
pub const TEMPLATES_VIEW: &str = "templates.view";
pub const ARTIFACTS_VIEW: &str = "artifacts.view";
pub const ARTIFACTS_UPLOAD: &str = "artifacts.upload";
pub const RBAC_ACCOUNTS_VIEW: &str = "rbac.accounts.view";
pub const RBAC_ROLES_VIEW: &str = "rbac.roles.view";
pub const RBAC_PERMISSIONS_VIEW: &str = "rbac.permissions.view";
pub const RBAC_SESSIONS_VIEW: &str = "rbac.sessions.view";
pub const API_TOKENS_VIEW: &str = "api_tokens.view";
pub const API_TOKENS_MANAGE: &str = "api_tokens.manage";
pub const SETTINGS_VIEW: &str = "settings.view";
pub const SETTINGS_UPDATE: &str = "settings.update";
pub const AUDIT_VIEW: &str = "audit.view";
pub const PROFILE_VIEW: &str = "profile.view";

pub fn all_permissions() -> &'static [PermissionDef] {
    &[
        PermissionDef {
            key: DASHBOARD_VIEW,
            name: "查看总览",
            description: "访问部署控制台总览页。",
            resource_type: PermissionResourceType::Page,
            resource_key: "dashboard",
            module: "总览",
        },
        PermissionDef {
            key: APPS_VIEW,
            name: "查看应用",
            description: "查看应用列表和详情。",
            resource_type: PermissionResourceType::Page,
            resource_key: "apps",
            module: "应用",
        },
        PermissionDef {
            key: "apps.create",
            name: "创建应用",
            description: "创建新的部署应用。",
            resource_type: PermissionResourceType::Action,
            resource_key: "apps.create",
            module: "应用",
        },
        PermissionDef {
            key: "apps.update",
            name: "编辑应用",
            description: "编辑应用基础信息和配置。",
            resource_type: PermissionResourceType::Action,
            resource_key: "apps.update",
            module: "应用",
        },
        PermissionDef {
            key: APPS_STATUS,
            name: "启停应用",
            description: "停用或重新启用部署应用。",
            resource_type: PermissionResourceType::Action,
            resource_key: APPS_STATUS,
            module: "应用",
        },
        PermissionDef {
            key: SERVICES_VIEW,
            name: "查看服务",
            description: "查看服务列表、版本和运行状态。",
            resource_type: PermissionResourceType::Page,
            resource_key: "services",
            module: "服务",
        },
        PermissionDef {
            key: "services.deploy",
            name: "部署服务",
            description: "发起服务部署任务。",
            resource_type: PermissionResourceType::Action,
            resource_key: "services.deploy",
            module: "服务",
        },
        PermissionDef {
            key: SERVICES_LOGS,
            name: "查看服务日志",
            description: "查看 Compose 或 systemd 服务最近日志。",
            resource_type: PermissionResourceType::Action,
            resource_key: SERVICES_LOGS,
            module: "服务",
        },
        PermissionDef {
            key: "services.rollback",
            name: "回滚服务",
            description: "回滚服务到历史版本。",
            resource_type: PermissionResourceType::Action,
            resource_key: "services.rollback",
            module: "服务",
        },
        PermissionDef {
            key: NODES_VIEW,
            name: "查看节点",
            description: "查看部署目标机器。",
            resource_type: PermissionResourceType::Page,
            resource_key: "nodes",
            module: "节点",
        },
        PermissionDef {
            key: NODES_MANAGE,
            name: "管理节点",
            description: "新增、编辑和探测部署目标机器。",
            resource_type: PermissionResourceType::Action,
            resource_key: "nodes.manage",
            module: "节点",
        },
        PermissionDef {
            key: NODES_INSTALL,
            name: "安装节点组件",
            description: "在节点上安装 Docker、Compose、Caddy 或 Nginx 等组件。",
            resource_type: PermissionResourceType::Action,
            resource_key: NODES_INSTALL,
            module: "节点",
        },
        PermissionDef {
            key: NODE_CREDENTIALS_VIEW,
            name: "查看节点凭据",
            description: "查看用于 SSH 节点免密登录的公钥、指纹和绑定关系。",
            resource_type: PermissionResourceType::Page,
            resource_key: "node_credentials",
            module: "节点",
        },
        PermissionDef {
            key: NODE_CREDENTIALS_MANAGE,
            name: "管理节点凭据",
            description: "生成或录入 SSH 密钥，并维护节点凭据状态。",
            resource_type: PermissionResourceType::Action,
            resource_key: NODE_CREDENTIALS_MANAGE,
            module: "节点",
        },
        PermissionDef {
            key: TASKS_VIEW,
            name: "查看部署任务",
            description: "查看部署任务队列和历史记录。",
            resource_type: PermissionResourceType::Page,
            resource_key: "tasks",
            module: "部署任务",
        },
        PermissionDef {
            key: "tasks.cancel",
            name: "取消任务",
            description: "取消尚未开始执行的排队任务。",
            resource_type: PermissionResourceType::Action,
            resource_key: "tasks.cancel",
            module: "部署任务",
        },
        PermissionDef {
            key: TASKS_RETRY,
            name: "重试任务",
            description: "从失败的部署任务重新发起一次任务。",
            resource_type: PermissionResourceType::Action,
            resource_key: TASKS_RETRY,
            module: "部署任务",
        },
        PermissionDef {
            key: TEMPLATES_VIEW,
            name: "查看模板",
            description: "查看 Docker Compose 和二进制部署模板。",
            resource_type: PermissionResourceType::Page,
            resource_key: "templates",
            module: "模板",
        },
        PermissionDef {
            key: ARTIFACTS_VIEW,
            name: "查看制品",
            description: "查看构建制品和版本。",
            resource_type: PermissionResourceType::Page,
            resource_key: "artifacts",
            module: "制品",
        },
        PermissionDef {
            key: ARTIFACTS_UPLOAD,
            name: "上传制品",
            description: "上传二进制制品并登记新版本。",
            resource_type: PermissionResourceType::Action,
            resource_key: ARTIFACTS_UPLOAD,
            module: "制品",
        },
        PermissionDef {
            key: RBAC_ACCOUNTS_VIEW,
            name: "查看账号",
            description: "查看后台账号列表。",
            resource_type: PermissionResourceType::Page,
            resource_key: "rbac.accounts",
            module: "权限",
        },
        PermissionDef {
            key: "rbac.accounts.manage",
            name: "管理账号",
            description: "创建、禁用、重置密码和分配角色。",
            resource_type: PermissionResourceType::Action,
            resource_key: "rbac.accounts.manage",
            module: "权限",
        },
        PermissionDef {
            key: RBAC_ROLES_VIEW,
            name: "查看角色",
            description: "查看角色和权限配置。",
            resource_type: PermissionResourceType::Page,
            resource_key: "rbac.roles",
            module: "权限",
        },
        PermissionDef {
            key: RBAC_PERMISSIONS_VIEW,
            name: "查看权限",
            description: "查看平台版本维护的权限注册表和权限说明。",
            resource_type: PermissionResourceType::Page,
            resource_key: "rbac.permissions",
            module: "权限",
        },
        PermissionDef {
            key: "rbac.roles.manage",
            name: "管理角色",
            description: "创建角色并分配权限。",
            resource_type: PermissionResourceType::Action,
            resource_key: "rbac.roles.manage",
            module: "权限",
        },
        PermissionDef {
            key: PROFILE_VIEW,
            name: "查看个人中心",
            description: "查看当前登录账号信息和修改密码。",
            resource_type: PermissionResourceType::Page,
            resource_key: "profile",
            module: "个人",
        },
        PermissionDef {
            key: "profile.password.change",
            name: "修改个人密码",
            description: "修改当前登录账号密码。",
            resource_type: PermissionResourceType::Action,
            resource_key: "profile.password.change",
            module: "个人",
        },
        PermissionDef {
            key: SETTINGS_VIEW,
            name: "查看设置",
            description: "查看平台设置。",
            resource_type: PermissionResourceType::Page,
            resource_key: "settings",
            module: "设置",
        },
        PermissionDef {
            key: SETTINGS_UPDATE,
            name: "修改平台设置",
            description: "修改部署默认值和平台可持久化配置。",
            resource_type: PermissionResourceType::Action,
            resource_key: SETTINGS_UPDATE,
            module: "设置",
        },
        PermissionDef {
            key: AUDIT_VIEW,
            name: "查看审计日志",
            description: "查看账号和部署相关审计日志。",
            resource_type: PermissionResourceType::Page,
            resource_key: "audit",
            module: "审计",
        },
        PermissionDef {
            key: RBAC_SESSIONS_VIEW,
            name: "查看会话",
            description: "查看后台登录会话。",
            resource_type: PermissionResourceType::Page,
            resource_key: "rbac.sessions",
            module: "权限",
        },
        PermissionDef {
            key: "rbac.sessions.manage",
            name: "管理会话",
            description: "强制下线后台登录会话。",
            resource_type: PermissionResourceType::Action,
            resource_key: "rbac.sessions.manage",
            module: "权限",
        },
        PermissionDef {
            key: API_TOKENS_VIEW,
            name: "View API Tokens",
            description: "View API tokens used by developers and AI callers.",
            resource_type: PermissionResourceType::Page,
            resource_key: "api_tokens",
            module: "Open API",
        },
        PermissionDef {
            key: API_TOKENS_MANAGE,
            name: "Manage API Tokens",
            description: "Create and revoke API tokens for open interfaces.",
            resource_type: PermissionResourceType::Action,
            resource_key: API_TOKENS_MANAGE,
            module: "Open API",
        },
    ]
}

pub fn permission_dependencies(permission_key: &str) -> &'static [&'static str] {
    match permission_key {
        "apps.create" | "apps.update" | APPS_STATUS => &[APPS_VIEW],
        "services.deploy" | "services.rollback" => &[APPS_VIEW, SERVICES_VIEW, TASKS_VIEW],
        SERVICES_LOGS => &[SERVICES_VIEW],
        NODES_MANAGE => &[NODES_VIEW],
        NODES_INSTALL => &[NODES_VIEW, TASKS_VIEW],
        NODE_CREDENTIALS_MANAGE => &[NODE_CREDENTIALS_VIEW],
        "tasks.cancel" | TASKS_RETRY => &[TASKS_VIEW],
        ARTIFACTS_UPLOAD => &[ARTIFACTS_VIEW],
        "rbac.accounts.manage" => &[RBAC_ACCOUNTS_VIEW],
        "rbac.roles.manage" => &[RBAC_ROLES_VIEW, RBAC_PERMISSIONS_VIEW],
        "rbac.sessions.manage" => &[RBAC_SESSIONS_VIEW],
        API_TOKENS_MANAGE => &[API_TOKENS_VIEW],
        "profile.password.change" => &[PROFILE_VIEW],
        SETTINGS_UPDATE => &[SETTINGS_VIEW],
        _ => &[],
    }
}

pub fn nav_permission(path: &str) -> Option<&'static str> {
    match path {
        "/" => Some(DASHBOARD_VIEW),
        "/apps" => Some(APPS_VIEW),
        "/services" => Some(SERVICES_VIEW),
        "/nodes" => Some(NODES_VIEW),
        "/node-credentials" => Some(NODE_CREDENTIALS_VIEW),
        "/tasks" => Some(TASKS_VIEW),
        "/templates" => Some(TEMPLATES_VIEW),
        "/artifacts" => Some(ARTIFACTS_VIEW),
        "/admin/accounts" => Some(RBAC_ACCOUNTS_VIEW),
        "/admin/roles" => Some(RBAC_ROLES_VIEW),
        "/admin/permissions" => Some(RBAC_PERMISSIONS_VIEW),
        "/admin/sessions" => Some(RBAC_SESSIONS_VIEW),
        "/admin/api-tokens" => Some(API_TOKENS_VIEW),
        "/profile" => Some(PROFILE_VIEW),
        "/settings" => Some(SETTINGS_VIEW),
        "/audit" => Some(AUDIT_VIEW),
        _ => None,
    }
}
