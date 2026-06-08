mod templates;

use std::sync::Arc;

use axum::{
    Form, Json, Router,
    extract::{FromRequestParts, Multipart, Path, Query, RawForm, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::{
    Settings,
    apps::{
        AppDeployDiffStatus, AppError, AppService, BinaryTaskAction, ComposeTaskAction,
        CreateAppInput, ServiceTargetNodeItem, UpdateAppConfigInput, UpdateAppMetadataInput,
        UploadBinaryArtifactInput, normalize_deploy_strategy,
    },
    auth::{
        API_TOKENS_MANAGE, API_TOKENS_VIEW, APPS_STATUS, APPS_VIEW, ARTIFACTS_UPLOAD,
        ARTIFACTS_VIEW, AUDIT_VIEW, AuditLogFilter, AuthService, CurrentSession, DASHBOARD_VIEW,
        LoginInput, NODES_INSTALL, NODES_MANAGE, NODES_VIEW, PROFILE_VIEW, RBAC_ACCOUNTS_VIEW,
        RBAC_PERMISSIONS_VIEW, RBAC_ROLES_VIEW, RBAC_SESSIONS_VIEW, SERVICES_LOGS, SERVICES_VIEW,
        SETTINGS_UPDATE, SETTINGS_VIEW, SessionTokens, TASKS_RETRY, TASKS_VIEW, TEMPLATES_VIEW,
        nav_permission, permission_dependencies,
    },
    catalog::{RenderTemplateInput, compose_templates, render_compose_template},
    health::{HealthCheckKind, normalize_health_config},
    node_credentials::{
        CreateGeneratedCredentialInput, CreateUploadedCredentialInput, NodeCredentialError,
        NodeCredentialService,
    },
    nodes::{CreateNodeInput, NodeError, NodeInstallComponent, NodeService, UpdateNodeInput},
    platform::{PlatformConfigError, PlatformConfigService, UpdatePlatformConfigInput},
    tasks::{TaskError, TaskListFilter, TaskService},
};
use templates::{
    AccountRow, AccountsTemplate, ApiTokenPageRow, ApiTokensTemplate, AppConfigSnapshotRow,
    AppDeployDiffRow, AppDeployDiffView, AppDeploymentRunRow, AppDetailTemplate, AppNodeChoiceRow,
    AppPageRow, AppRow, AppRuntimeStateRow, AppTargetChoiceRow, AppsTemplate, ArtifactPageRow,
    ArtifactsTemplate, AuditFilterOptionRow, AuditLogRow, AuditTemplate, BinaryReleaseRow,
    ComposeResultView, DashboardTemplate, DeployConfirmTargetNodeRow, DeployConfirmTemplate,
    DeployPlanFileRow, DeployPlanStepRow, DeployPreflightActionRow, DeployPreflightCheckRow,
    DeployPreflightRow, LoginTemplate, NavItem, NavSection, NodeAppRuntimeRow,
    NodeCapabilityGuideRow, NodeCheckHistoryRow, NodeCredentialOptionRow, NodeCredentialPageRow,
    NodeCredentialsTemplate, NodeDetailModalRow, NodeDetailTemplate, NodePageRow, NodeRow,
    NodeTaskRow, NodesTemplate, PermissionGroup, PermissionRow, PermissionsTemplate,
    ProfileTemplate, RbacFilterOptionRow, RoleRow, RolesTemplate, ServiceLogTailOptionRow,
    ServiceLogsTemplate, ServiceNodeLinkRow, ServicePageRow, ServicesTemplate, SessionRow,
    SessionsTemplate, SettingsRow, SettingsTemplate, SummaryItem, TaskAppFilterRow,
    TaskDetailTemplate, TaskDetailView, TaskExecutionGuideView, TaskFilterOptionRow, TaskLogRow,
    TaskNodeResultRow, TaskPageRow, TaskPhaseStepRow, TaskReturnActionView, TaskRow, TasksTemplate,
    TemplateCardRow, TemplatesTemplate, render_html,
};

const LOGO_SVG: &str = include_str!("../../assets/logo.svg");
const APP_JS: &str = include_str!("../../assets/app.js");
const ASSET_VERSION: &str = "20260605-node-check-row";

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

pub struct AppStateServices {
    pub auth: AuthService,
    pub nodes: NodeService,
    pub node_credentials: NodeCredentialService,
    pub apps: AppService,
    pub tasks: TaskService,
    pub platform: PlatformConfigService,
}

struct AppStateInner {
    settings: Settings,
    db: SqlitePool,
    auth: AuthService,
    nodes: NodeService,
    node_credentials: NodeCredentialService,
    apps: AppService,
    tasks: TaskService,
    platform: PlatformConfigService,
}

impl AppState {
    pub fn new(settings: Settings, db: SqlitePool, services: AppStateServices) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                settings,
                db,
                auth: services.auth,
                nodes: services.nodes,
                node_credentials: services.node_credentials,
                apps: services.apps,
                tasks: services.tasks,
                platform: services.platform,
            }),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    pub fn db(&self) -> &SqlitePool {
        &self.inner.db
    }

    pub fn auth(&self) -> &AuthService {
        &self.inner.auth
    }

    pub fn nodes(&self) -> &NodeService {
        &self.inner.nodes
    }

    pub fn node_credentials(&self) -> &NodeCredentialService {
        &self.inner.node_credentials
    }

    pub fn apps(&self) -> &AppService {
        &self.inner.apps
    }

    pub fn tasks(&self) -> &TaskService {
        &self.inner.tasks
    }

    pub fn platform(&self) -> &PlatformConfigService {
        &self.inner.platform
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login_submit))
        .route("/auth/refresh", post(refresh_submit))
        .route("/logout", post(logout_submit))
        .route("/apps/new", get(new_app_redirect))
        .route("/apps", get(apps_page))
        .route("/apps", post(create_app_submit))
        .route("/apps/{app_id}", get(app_detail_page))
        .route("/apps/{app_id}/status", post(app_status_submit))
        .route("/apps/{app_id}/metadata", post(app_metadata_submit))
        .route("/apps/{app_id}/config", post(app_config_submit))
        .route(
            "/apps/{app_id}/snapshots/{snapshot_id}/restore",
            post(app_snapshot_restore_submit),
        )
        .route(
            "/apps/{app_id}/compose/config",
            post(app_compose_config_submit),
        )
        .route("/apps/{app_id}/compose/logs", post(app_compose_logs_submit))
        .route(
            "/apps/{app_id}/compose/{action}/confirm",
            get(app_compose_confirm_page),
        )
        .route("/apps/{app_id}/compose/up", post(app_compose_up_submit))
        .route("/apps/{app_id}/compose/down", post(app_compose_down_submit))
        .route(
            "/apps/{app_id}/compose/restart",
            post(app_compose_restart_submit),
        )
        .route(
            "/apps/{app_id}/binary/{action}/confirm",
            get(app_binary_confirm_page),
        )
        .route(
            "/apps/{app_id}/binary/restart",
            post(app_binary_restart_submit),
        )
        .route("/apps/{app_id}/binary/stop", post(app_binary_stop_submit))
        .route(
            "/apps/{app_id}/binary/upload",
            post(app_binary_upload_submit),
        )
        .route(
            "/apps/{app_id}/binary/releases/{artifact_id}/activate",
            post(app_binary_release_activate_submit),
        )
        .route("/services", get(services_page))
        .route(
            "/services/{app_id}/{service_name}/logs",
            get(service_logs_page),
        )
        .route("/nodes", get(nodes_page))
        .route("/nodes/{node_id}", get(node_detail_page))
        .route("/nodes", post(create_node_submit))
        .route("/nodes/update", post(node_update_submit))
        .route("/nodes/status", post(node_status_submit))
        .route("/nodes/check", post(node_check_submit))
        .route("/nodes/install", post(node_install_submit))
        .route("/node-credentials", get(node_credentials_page))
        .route(
            "/node-credentials/generate",
            post(node_credential_generate_submit),
        )
        .route(
            "/node-credentials/upload",
            post(node_credential_upload_submit),
        )
        .route(
            "/node-credentials/status",
            post(node_credential_status_submit),
        )
        .route("/tasks", get(tasks_page))
        .route("/tasks/{task_id}", get(task_detail_page))
        .route("/tasks/{task_id}/cancel", post(task_cancel_submit))
        .route("/tasks/{task_id}/retry", post(task_retry_submit))
        .route("/templates", get(templates_page))
        .route("/templates", post(create_template_app_submit))
        .route("/artifacts", get(artifacts_page))
        .route("/admin/accounts", get(accounts_page))
        .route("/admin/accounts", post(create_account_submit))
        .route("/admin/accounts/status", post(account_status_submit))
        .route("/admin/accounts/password", post(account_password_submit))
        .route("/admin/accounts/roles", post(account_roles_submit))
        .route("/admin/roles", get(roles_page))
        .route("/admin/roles", post(create_role_submit))
        .route("/admin/roles/status", post(role_status_submit))
        .route("/admin/roles/permissions", post(role_permissions_submit))
        .route("/admin/permissions", get(permissions_page))
        .route("/admin/sessions", get(sessions_page))
        .route("/admin/sessions/revoke", post(session_revoke_submit))
        .route("/admin/api-tokens", get(api_tokens_page))
        .route("/admin/api-tokens", post(api_token_create_submit))
        .route("/admin/api-tokens/revoke", post(api_token_revoke_submit))
        .route("/profile", get(profile_page))
        .route("/profile/password", post(profile_password_submit))
        .route("/settings", get(settings_page).post(settings_submit))
        .route("/audit", get(audit_page))
        .route("/openapi.json", get(openapi_json))
        .route("/docs/openapi", get(openapi_docs))
        .route("/api/v1/nodes", get(api_v1_nodes))
        .route("/api/v1/apps", get(api_v1_apps).post(api_v1_create_app))
        .route("/api/v1/apps/{app_id}", get(api_v1_app_detail))
        .route("/api/v1/apps/{app_id}/deploy", post(api_v1_deploy_app))
        .route("/api/v1/tasks", get(api_v1_tasks))
        .route("/api/v1/tasks/{task_id}", get(api_v1_task_detail))
        .route("/healthz", get(healthz))
        .route("/assets/logo.svg", get(logo_svg))
        .route("/assets/app.js", get(app_js))
        .route("/favicon.svg", get(logo_svg))
        .route("/favicon.ico", get(logo_svg))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn dashboard(State(state): State<AppState>, session: CurrentSession) -> Response {
    if !session.can(DASHBOARD_VIEW) {
        return forbidden();
    }
    let nav_sections = nav_sections("/", &session);
    let apps = match state.apps().list_apps().await {
        Ok(apps) => apps,
        Err(err) => return app_error_response(err),
    };
    let services = match state.apps().list_services().await {
        Ok(services) => services,
        Err(err) => return app_error_response(err),
    };
    let nodes = match state.nodes().list_nodes().await {
        Ok(nodes) => nodes,
        Err(err) => return node_error_response(err),
    };
    let tasks = match state.tasks().list_tasks().await {
        Ok(tasks) => tasks,
        Err(err) => return task_error_response(err),
    };

    let running_tasks = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "queued" | "running"))
        .count();
    let online_nodes = nodes.iter().filter(|node| node.status == "online").count();
    let healthy_services = services
        .iter()
        .filter(|service| service.runtime_status == "healthy")
        .count();
    let summary_items = vec![
        SummaryItem {
            label: "应用",
            value: apps.len().to_string(),
            detail: format!(
                "{} 个运行中，{} 个需要处理",
                count_apps(&apps, "running"),
                count_apps(&apps, "failed")
            ),
            tone: "neutral",
        },
        SummaryItem {
            label: "运行项",
            value: services.len().to_string(),
            detail: format!("{healthy_services} 个节点健康"),
            tone: if healthy_services == services.len() {
                "neutral"
            } else {
                "warning"
            },
        },
        SummaryItem {
            label: "节点",
            value: nodes.len().to_string(),
            detail: format!(
                "{online_nodes} 个在线，{} 个待处理",
                nodes.len().saturating_sub(online_nodes)
            ),
            tone: "neutral",
        },
        SummaryItem {
            label: "运行任务",
            value: running_tasks.to_string(),
            detail: format!("最近记录 {} 条任务", tasks.len()),
            tone: if running_tasks > 0 {
                "active"
            } else {
                "neutral"
            },
        },
    ];
    let app_rows = apps
        .iter()
        .take(5)
        .map(|app| AppRow {
            name: app.name.clone(),
            stack: app_type_label(&app.app_type).to_owned(),
            services: dashboard_services_text(&services, app.id),
            target: app
                .target_names
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("未绑定节点")
                .to_owned(),
            status: app_status_label(&app.status),
            status_tone: app_status_tone(&app.status),
            updated_at: app.updated_at.clone(),
        })
        .collect::<Vec<_>>();
    let node_rows = nodes
        .iter()
        .take(5)
        .map(|node| NodeRow {
            name: node.name.clone(),
            address: node.address.clone(),
            region: if node.region.is_empty() {
                "未分区".to_owned()
            } else {
                node.region.clone()
            },
            load: node.docker_status.clone(),
            status: node_status_label(&node.status),
            status_tone: node_status_tone(&node.status),
        })
        .collect::<Vec<_>>();
    let task_rows = tasks
        .iter()
        .take(5)
        .map(|task| TaskRow {
            title: task.title.clone(),
            target: task
                .app_name
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("未关联应用")
                .to_owned(),
            status: task_status_label(&task.status),
            status_tone: task_status_tone(&task.status),
            time: task.updated_at.clone(),
        })
        .collect::<Vec<_>>();

    render_html(DashboardTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        app_rows: &app_rows,
        node_rows: &node_rows,
        task_rows: &task_rows,
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LoginQuery {
    notice: Option<String>,
}

async fn login_page(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Response {
    let bootstrap_required = match state.auth().bootstrap_required().await {
        Ok(required) => required,
        Err(err) => return err.into_response(),
    };
    let notice_message = login_notice_message(query.notice.as_deref());
    render_html(LoginTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        bootstrap_required,
        error_message: notice_message,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct CreateAppForm {
    csrf_token: String,
    app_key: String,
    name: String,
    description: String,
    app_type: String,
    #[serde(default)]
    deploy_strategy: String,
    work_dir: String,
    compose_content: String,
    env_content: String,
    #[serde(default)]
    binary_artifact_version: String,
    #[serde(default)]
    binary_artifact_path: String,
    #[serde(default)]
    binary_exec_args: String,
    #[serde(default)]
    binary_service_user: String,
    #[serde(default)]
    binary_unit_name: String,
    #[serde(default)]
    binary_release_strategy: String,
    #[serde(default)]
    binary_active_slot: String,
    #[serde(default)]
    binary_base_port: i64,
    #[serde(default)]
    binary_standby_port: i64,
    #[serde(default)]
    binary_proxy_enabled: bool,
    #[serde(default)]
    binary_proxy_kind: String,
    #[serde(default)]
    binary_proxy_domain: String,
    #[serde(default)]
    binary_proxy_config_path: String,
    target_node_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct CreateTemplateAppForm {
    csrf_token: String,
    template_key: String,
    app_key: String,
    name: String,
    description: String,
    work_dir: String,
    #[serde(default)]
    deploy_strategy: String,
    port: u16,
    target_node_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct UpdateAppConfigForm {
    csrf_token: String,
    compose_content: String,
    env_content: String,
    #[serde(default)]
    binary_artifact_version: String,
    #[serde(default)]
    binary_artifact_path: String,
    #[serde(default)]
    binary_exec_args: String,
    #[serde(default)]
    binary_service_user: String,
    #[serde(default)]
    binary_unit_name: String,
    #[serde(default)]
    binary_release_strategy: String,
    #[serde(default)]
    binary_active_slot: String,
    #[serde(default)]
    binary_base_port: i64,
    #[serde(default)]
    binary_standby_port: i64,
    #[serde(default)]
    binary_proxy_enabled: bool,
    #[serde(default)]
    binary_proxy_kind: String,
    #[serde(default)]
    binary_proxy_domain: String,
    #[serde(default)]
    binary_proxy_config_path: String,
    health_check_kind: String,
    health_endpoint: String,
    health_timeout_secs: i64,
    health_expected_status: i64,
}

#[derive(Deserialize)]
struct UpdateAppMetadataForm {
    csrf_token: String,
    name: String,
    description: String,
    work_dir: String,
    #[serde(default)]
    deploy_strategy: String,
    target_node_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct AppStatusForm {
    csrf_token: String,
    status: String,
}

#[derive(Deserialize)]
struct CreateAccountForm {
    csrf_token: String,
    username: String,
    display_name: String,
    password: String,
    role_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct AccountStatusForm {
    csrf_token: String,
    account_id: i64,
    status: String,
}

#[derive(Deserialize)]
struct AccountPasswordForm {
    csrf_token: String,
    account_id: i64,
    password: String,
}

#[derive(Deserialize)]
struct AccountRolesForm {
    csrf_token: String,
    account_id: i64,
    role_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct CreateRoleForm {
    csrf_token: String,
    role_code: String,
    role_name: String,
    description: String,
    permission_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct RoleStatusForm {
    csrf_token: String,
    role_id: i64,
    status: String,
}

#[derive(Deserialize)]
struct RolePermissionsForm {
    csrf_token: String,
    role_id: i64,
    permission_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct ProfilePasswordForm {
    csrf_token: String,
    current_password: String,
    new_password: String,
}

#[derive(Deserialize)]
struct SessionRevokeForm {
    csrf_token: String,
    session_id: i64,
}

#[derive(Deserialize)]
struct CreateApiTokenForm {
    csrf_token: String,
    source: String,
}

#[derive(Deserialize)]
struct ApiTokenRevokeForm {
    csrf_token: String,
    token_id: i64,
}

#[derive(Deserialize)]
struct CreateNodeForm {
    csrf_token: String,
    node_key: String,
    name: String,
    node_type: String,
    address: String,
    ssh_port: i64,
    ssh_user: String,
    credential_id: Option<i64>,
    work_dir: String,
    region: String,
    labels: String,
}

#[derive(Deserialize)]
struct SettingsForm {
    csrf_token: String,
    default_app_work_dir: String,
    default_node_work_dir: String,
    uploaded_binary_releases_to_keep: usize,
}

#[derive(Deserialize)]
struct UpdateNodeForm {
    csrf_token: String,
    node_id: i64,
    name: String,
    node_type: String,
    address: String,
    ssh_port: i64,
    ssh_user: String,
    credential_id: Option<i64>,
    work_dir: String,
    region: String,
    labels: String,
}

#[derive(Deserialize)]
struct GenerateNodeCredentialForm {
    csrf_token: String,
    name: String,
    key_algorithm: String,
}

#[derive(Deserialize)]
struct UploadNodeCredentialForm {
    csrf_token: String,
    name: String,
    private_key: String,
    public_key: String,
    passphrase_hint: String,
}

#[derive(Deserialize)]
struct NodeCredentialStatusForm {
    csrf_token: String,
    credential_id: i64,
    status: String,
}

#[derive(Deserialize)]
struct NodeStatusForm {
    csrf_token: String,
    node_id: i64,
    status: String,
}

#[derive(Deserialize)]
struct NodeCheckForm {
    csrf_token: String,
    node_id: i64,
    return_to: Option<String>,
}

#[derive(Serialize)]
struct NodeCheckAjaxResponse<'a> {
    status: &'a str,
    status_tone: &'a str,
    docker_status: &'a str,
    message: &'a str,
}

#[derive(Deserialize)]
struct NodeInstallForm {
    csrf_token: String,
    node_id: i64,
    component: String,
    return_to: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TaskListQuery {
    status: Option<String>,
    phase: Option<String>,
    app_id: Option<i64>,
    task_kind: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TaskDetailQuery {
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct TaskRetryForm {
    csrf_token: String,
    return_to: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AuditLogQuery {
    action: Option<String>,
    target_type: Option<String>,
    actor: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AccountsQuery {
    status: Option<String>,
    role: Option<String>,
    q: Option<String>,
    notice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct RolesQuery {
    status: Option<String>,
    module: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PermissionsQuery {
    module: Option<String>,
    resource_type: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct SessionsQuery {
    status: Option<String>,
    q: Option<String>,
    notice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApiTokensQuery {
    notice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AppsQuery {
    r#type: Option<String>,
    status: Option<String>,
    q: Option<String>,
    page: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AppDetailQuery {
    notice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct NodesQuery {
    r#type: Option<String>,
    status: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApiV1TaskListQuery {
    status: Option<String>,
    phase: Option<String>,
    app_id: Option<i64>,
    task_kind: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ApiV1CreateAppRequest {
    app_key: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default = "default_app_type")]
    app_type: String,
    #[serde(default)]
    deploy_strategy: String,
    #[serde(default)]
    work_dir: String,
    #[serde(default)]
    target_node_ids: Vec<i64>,
    #[serde(default)]
    compose_content: String,
    #[serde(default)]
    env_content: String,
    #[serde(default)]
    binary_artifact_version: String,
    #[serde(default)]
    binary_artifact_path: String,
    #[serde(default)]
    binary_exec_args: String,
    #[serde(default)]
    binary_service_user: String,
    #[serde(default)]
    binary_unit_name: String,
    #[serde(default)]
    binary_release_strategy: String,
    #[serde(default)]
    binary_active_slot: String,
    #[serde(default)]
    binary_base_port: i64,
    #[serde(default)]
    binary_standby_port: i64,
    #[serde(default)]
    binary_proxy_enabled: bool,
    #[serde(default)]
    binary_proxy_kind: String,
    #[serde(default)]
    binary_proxy_domain: String,
    #[serde(default)]
    binary_proxy_config_path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ApiV1DeployAppRequest {
    action: String,
}

#[derive(Serialize)]
struct ApiErrorBody<'a> {
    error: &'a str,
}

fn default_app_type() -> String {
    "compose".to_owned()
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ArtifactsQuery {
    status: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ServiceLogsQuery {
    node_id: Option<i64>,
    tail: Option<u16>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ServicesQuery {
    kind: Option<String>,
    status: Option<String>,
    q: Option<String>,
}

async fn login_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let bootstrap_required = match state.auth().bootstrap_required().await {
        Ok(required) => required,
        Err(err) => return err.into_response(),
    };
    let input = LoginInput {
        username: form.username,
        password: form.password,
        display_name: form.display_name,
        client_ip: String::new(),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    };
    let result = if bootstrap_required {
        state.auth().bootstrap_init(input).await
    } else {
        state.auth().login(input).await
    };
    match result {
        Ok(result) => {
            redirect_with_auth_cookies("/", &result.tokens, state.settings().cookie_secure)
        }
        Err(err) => render_html(LoginTemplate {
            product_name: "Easy Deploy",
            css: include_str!("../../assets/app.css"),
            asset_version: ASSET_VERSION,
            release_version: concat!("v", env!("CARGO_PKG_VERSION")),
            bootstrap_required,
            error_message: Some(err.message()),
        }),
    }
}

async fn logout_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if let Err(err) = state.auth().logout(&session).await {
        return err.into_response();
    }
    redirect_with_expired_auth_cookies("/login?notice=logout", state.settings().cookie_secure)
}

async fn refresh_submit(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(refresh_token) = cookie_value(&headers, "ed_refresh") else {
        return redirect_with_expired_auth_cookies(
            "/login?notice=expired",
            state.settings().cookie_secure,
        );
    };
    match state.auth().refresh(&refresh_token).await {
        Ok(result) => {
            redirect_with_auth_cookies("/", &result.tokens, state.settings().cookie_secure)
        }
        Err(_) => redirect_with_expired_auth_cookies(
            "/login?notice=expired",
            state.settings().cookie_secure,
        ),
    }
}

async fn apps_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<AppsQuery>,
) -> Response {
    if !session.can(APPS_VIEW) {
        return forbidden();
    }
    let apps = match state.apps().list_apps().await {
        Ok(apps) => apps,
        Err(err) => return app_error_response(err),
    };
    let node_options = match state.apps().node_options().await {
        Ok(nodes) => nodes,
        Err(err) => return app_error_response(err),
    };
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let selected_type = normalize_app_type_filter(query.r#type.as_deref());
    let selected_status = normalize_app_status_filter(query.status.as_deref());
    let search_query = query.q.unwrap_or_default().trim().to_owned();
    let filtered_apps = apps
        .iter()
        .filter(|app| app_matches_filters(app, selected_type, selected_status, &search_query))
        .collect::<Vec<_>>();
    let total_count = filtered_apps.len();
    let page_size = 10usize;
    let total_pages = total_count.div_ceil(page_size).max(1);
    let page = normalize_page(query.page, total_pages);
    let page_start_index = (page - 1) * page_size;
    let page_end_index = (page_start_index + page_size).min(total_count);
    let rows = filtered_apps[page_start_index..page_end_index]
        .iter()
        .map(|app| AppPageRow {
            id: app.id,
            name: &app.name,
            app_key: &app.app_key,
            description: if app.description.is_empty() {
                "暂无描述"
            } else {
                &app.description
            },
            app_type: app_type_label(&app.app_type),
            deploy_strategy: deploy_strategy_label(&app.deploy_strategy),
            work_dir: &app.work_dir,
            status: app_status_label(&app.status),
            status_tone: app_status_tone(&app.status),
            targets: app.target_names.as_deref().unwrap_or("未绑定节点"),
            target_count: app.target_count,
            updated_at: &app.updated_at,
            created_at: &app.created_at,
            toggle_status: app_status_toggle_value(&app.status),
            toggle_label: app_status_toggle_label(&app.status),
        })
        .collect::<Vec<_>>();
    let node_choices = node_options
        .iter()
        .map(|node| AppNodeChoiceRow {
            id: node.id,
            label: &node.name,
            detail: &node.node_key,
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/apps", &session);
    render_html(AppsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        apps: &rows,
        node_choices: &node_choices,
        selected_type,
        selected_status,
        query: &search_query,
        filtered_count: total_count,
        page,
        total_pages,
        page_start: if total_count == 0 {
            0
        } else {
            page_start_index + 1
        },
        page_end: page_end_index,
        prev_page_href: app_page_href(selected_type, selected_status, &search_query, page - 1),
        next_page_href: app_page_href(selected_type, selected_status, &search_query, page + 1),
        has_prev_page: page > 1,
        has_next_page: page < total_pages,
        default_app_work_dir: &platform_config.default_app_work_dir_for("orders-api"),
        default_app_work_dir_template: &platform_config.default_app_work_dir,
        can_manage: session.can("apps.create"),
        can_toggle_status: session.can(APPS_STATUS),
    })
}

async fn new_app_redirect() -> Response {
    redirect("/apps#create-app-modal")
}

async fn create_app_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_create_app_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if let Err(err) = normalize_deploy_strategy(&form.deploy_strategy) {
        return (StatusCode::BAD_REQUEST, err.message().to_owned()).into_response();
    }
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("apps.create") {
        return forbidden();
    }
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let work_dir = if form.work_dir.trim().is_empty() {
        platform_config.default_app_work_dir_for(&form.app_key)
    } else {
        form.work_dir
    };
    let input = CreateAppInput {
        app_key: form.app_key,
        name: form.name,
        description: form.description,
        app_type: form.app_type,
        deploy_strategy: form.deploy_strategy,
        work_dir,
        compose_content: form.compose_content,
        env_content: form.env_content,
        binary_artifact_version: form.binary_artifact_version,
        binary_artifact_path: form.binary_artifact_path,
        binary_exec_args: form.binary_exec_args,
        binary_service_user: form.binary_service_user,
        binary_unit_name: form.binary_unit_name,
        binary_release_strategy: form.binary_release_strategy,
        binary_active_slot: form.binary_active_slot,
        binary_base_port: form.binary_base_port,
        binary_standby_port: form.binary_standby_port,
        binary_proxy_enabled: form.binary_proxy_enabled,
        binary_proxy_kind: form.binary_proxy_kind,
        binary_proxy_domain: form.binary_proxy_domain,
        binary_proxy_config_path: form.binary_proxy_config_path,
        target_node_ids: form.target_node_ids,
    };
    match state.apps().create_app(input).await {
        Ok(app_id) => redirect(&format!("/apps/{app_id}?notice=created")),
        Err(err) => app_error_response(err),
    }
}

async fn app_status_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<AppStatusForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(APPS_STATUS) {
        return forbidden();
    }
    match state.apps().set_app_status(app_id, &form.status).await {
        Ok(change) => {
            record_audit_event(
                &state,
                &session,
                "apps.status",
                "app",
                &change.app_id.to_string(),
                &format!(
                    "{} 状态 {} -> {}",
                    change.app_name,
                    app_status_label(&change.previous_status),
                    app_status_label(&change.status)
                ),
            )
            .await;
            redirect("/apps")
        }
        Err(err) => app_error_response(err),
    }
}

async fn app_detail_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Query(query): Query<AppDetailQuery>,
) -> Response {
    if !session.can(APPS_VIEW) {
        return forbidden();
    }
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(err) => return app_error_response(err),
    };
    render_app_detail(
        &session,
        detail,
        None,
        app_detail_notice_message(query.notice.as_deref()),
    )
}

fn render_app_detail(
    session: &CurrentSession,
    detail: crate::apps::AppConfigDetail,
    compose_result: Option<ComposeResultView>,
    notice: Option<&str>,
) -> Response {
    let nav_sections = nav_sections("/apps", session);
    let app_enabled = detail.app.status != "disabled";
    let app_idle = detail.app.status != "deploying";
    let deployment_runs = detail
        .deployment_runs
        .iter()
        .map(|run| AppDeploymentRunRow {
            task_id: run.task_id,
            title: run
                .task_title
                .clone()
                .unwrap_or_else(|| format!("部署记录 #{}", run.id)),
            action: deploy_action_label(&run.deploy_action),
            status: deployment_status_label(&run.status),
            status_tone: deployment_status_tone(&run.status),
            message: if run.message.is_empty() {
                "无摘要".to_owned()
            } else {
                run.message.clone()
            },
            started_at: run.started_at.clone(),
            finished_at: run
                .finished_at
                .clone()
                .unwrap_or_else(|| "未结束".to_owned()),
        })
        .collect::<Vec<_>>();
    let can_manage = session.can("apps.update") && app_enabled && app_idle;
    let can_upload_artifact = session.can(ARTIFACTS_UPLOAD) && app_enabled && app_idle;
    let can_rollback = session.can("services.rollback") && app_enabled && app_idle;
    let config_snapshots = detail
        .config_snapshots
        .iter()
        .map(|snapshot| AppConfigSnapshotRow {
            id: snapshot.id,
            kind: snapshot_kind_label(&snapshot.snapshot_kind),
            compose_summary: config_summary(&snapshot.compose_content),
            env_summary: config_summary(&snapshot.env_content),
            created_at: snapshot.created_at.clone(),
            can_restore: can_manage,
        })
        .collect::<Vec<_>>();
    let deploy_diff = deploy_diff_view(&detail.deploy_diff);
    let latest_task_href = deployment_runs
        .iter()
        .find_map(|run| run.task_id.map(|task_id| format!("/tasks/{task_id}")))
        .unwrap_or_default();
    let has_latest_task_href = !latest_task_href.is_empty();
    let runtime_states = detail
        .runtime_states
        .iter()
        .map(|state| {
            let log_links = detail
                .service_names
                .iter()
                .map(|service_name| ServiceNodeLinkRow {
                    name: service_name.clone(),
                    node_key: state.node_key.clone(),
                    href: service_log_href(detail.app.id, service_name, state.node_id, 200),
                    node_href: format!("/nodes/{}", state.node_id),
                    task_href: latest_task_href.clone(),
                    task_id: 0,
                    task_return_to: String::new(),
                    active: false,
                    runtime_status: "日志",
                    runtime_status_tone: "neutral",
                    runtime_summary: "查看该运行项日志".to_owned(),
                    task_status: "最近任务",
                    task_status_tone: "neutral",
                    task_action_label: "最近任务",
                    active_version: String::new(),
                    last_health_at: String::new(),
                    message: String::new(),
                    has_task_href: has_latest_task_href,
                    can_retry_task: false,
                })
                .collect::<Vec<_>>();
            AppRuntimeStateRow {
                node_name: state.node_name.clone(),
                node_key: state.node_key.clone(),
                node_detail_href: format!("/nodes/{}", state.node_id),
                task_href: latest_task_href.clone(),
                has_task_href: has_latest_task_href,
                has_log_links: !log_links.is_empty(),
                log_links,
                status: runtime_status_label(&state.runtime_status),
                status_tone: runtime_status_tone(&state.runtime_status),
                service_count: state.service_count,
                active_version: if state.active_version.is_empty() {
                    "未部署".to_owned()
                } else {
                    state.active_version.clone()
                },
                message: if state.message.is_empty() {
                    "暂无运行信息".to_owned()
                } else {
                    state.message.clone()
                },
                last_deploy_at: state
                    .last_deploy_at
                    .clone()
                    .unwrap_or_else(|| "未部署".to_owned()),
                updated_at: state.updated_at.clone(),
            }
        })
        .collect::<Vec<_>>();
    let target_choices = detail
        .target_choices
        .iter()
        .map(|node| AppTargetChoiceRow {
            id: node.id,
            label: node.name.clone(),
            detail: node.node_key.clone(),
            checked: node.checked,
        })
        .collect::<Vec<_>>();
    let binary_releases = detail
        .binary_releases
        .iter()
        .map(|release| {
            let status = artifact_status_label(&release.status);
            BinaryReleaseRow {
                id: release.id,
                version: release.version.clone(),
                artifact_kind: artifact_kind_label(&release.artifact_kind).to_owned(),
                status,
                status_tone: artifact_status_tone(&release.status),
                artifact_path: release.artifact_path.clone(),
                sha256: short_hash(&release.metadata_value("sha256")),
                size: format_size(&release.metadata_value("size_bytes")),
                entry_file: display_text(release.metadata_value("entry_file"), "未记录"),
                created_at: release.created_at.clone(),
                can_rollback: can_rollback && status != "当前",
            }
        })
        .collect::<Vec<_>>();
    let binary_unit_path = detail
        .binary_runtime
        .unit_path
        .to_string_lossy()
        .to_string();
    let binary_env_path = detail.binary_runtime.env_path.to_string_lossy().to_string();
    let binary_blue_unit_path = detail
        .binary_runtime
        .blue_unit_path
        .to_string_lossy()
        .to_string();
    let binary_blue_env_path = detail
        .binary_runtime
        .blue_env_path
        .to_string_lossy()
        .to_string();
    let binary_green_unit_path = detail
        .binary_runtime
        .green_unit_path
        .to_string_lossy()
        .to_string();
    let binary_green_env_path = detail
        .binary_runtime
        .green_env_path
        .to_string_lossy()
        .to_string();
    let binary_release_path = detail
        .binary_runtime
        .release_path
        .to_string_lossy()
        .to_string();
    let binary_current_path = detail
        .binary_runtime
        .current_path
        .to_string_lossy()
        .to_string();
    render_html(AppDetailTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        id: detail.app.id,
        name: &detail.app.name,
        app_key: &detail.app.app_key,
        description: &detail.app.description,
        app_type: app_type_label(&detail.app.app_type),
        is_binary_app: detail.app.app_type == "binary",
        deploy_strategy: detail.app.deploy_strategy.as_str(),
        deploy_strategy_label: deploy_strategy_label(&detail.app.deploy_strategy),
        work_dir: &detail.app.work_dir,
        runtime_root: &detail.runtime_root,
        status: app_status_label(&detail.app.status),
        status_tone: app_status_tone(&detail.app.status),
        targets: detail.app.target_names.as_deref().unwrap_or("未绑定节点"),
        target_count: detail.app.target_count,
        created_at: &detail.app.created_at,
        updated_at: &detail.app.updated_at,
        compose_content: &detail.compose_content,
        env_content: &detail.env_content,
        metadata_content: &detail.metadata_content,
        binary_artifact_version: &detail.binary_config.artifact_version,
        binary_artifact_path: &detail.binary_config.artifact_path,
        binary_exec_args: &detail.binary_config.exec_args,
        binary_service_user: &detail.binary_config.service_user,
        binary_unit_name: &detail.binary_config.unit_name,
        binary_release_strategy: &detail.binary_config.release_strategy,
        binary_release_strategy_label: binary_release_strategy_label(
            &detail.binary_config.release_strategy,
        ),
        binary_active_slot: &detail.binary_config.active_slot,
        binary_standby_slot: binary_standby_slot(&detail.binary_config.active_slot),
        binary_base_port: detail.binary_config.base_port,
        binary_standby_port: detail.binary_config.standby_port,
        binary_proxy_enabled: detail.binary_config.proxy_enabled == 1,
        binary_proxy_kind: &detail.binary_config.proxy_kind,
        binary_proxy_kind_label: binary_proxy_kind_label(&detail.binary_config.proxy_kind),
        binary_proxy_domain: &detail.binary_config.proxy_domain,
        binary_proxy_config_path: &detail.binary_config.proxy_config_path,
        binary_unit_path: &binary_unit_path,
        binary_env_path: &binary_env_path,
        binary_blue_unit_path: &binary_blue_unit_path,
        binary_blue_env_path: &binary_blue_env_path,
        binary_green_unit_path: &binary_green_unit_path,
        binary_green_env_path: &binary_green_env_path,
        binary_release_path: &binary_release_path,
        binary_current_path: &binary_current_path,
        binary_unit_content: &detail.binary_runtime.unit_content,
        binary_env_content: &detail.binary_runtime.env_content,
        binary_blue_unit_content: &detail.binary_runtime.blue_unit_content,
        binary_blue_env_content: &detail.binary_runtime.blue_env_content,
        binary_green_unit_content: &detail.binary_runtime.green_unit_content,
        binary_green_env_content: &detail.binary_runtime.green_env_content,
        binary_release_content: &detail.binary_runtime.release_content,
        binary_current_content: &detail.binary_runtime.current_content,
        health_check_kind: detail.health_check.kind.as_str(),
        health_check_label: detail.health_check.kind.label(),
        health_endpoint: &detail.health_check.endpoint,
        health_timeout_secs: detail.health_check.timeout_secs,
        health_expected_status: detail.health_check.expected_status,
        deployment_runs: &deployment_runs,
        config_snapshots: &config_snapshots,
        deploy_diff: &deploy_diff,
        runtime_states: &runtime_states,
        target_choices: &target_choices,
        binary_releases: &binary_releases,
        can_manage,
        can_upload_artifact,
        can_deploy: session.can("services.deploy") && app_enabled && app_idle,
        can_logs: session.can(SERVICES_LOGS),
        compose_result,
        notice,
    })
}

async fn app_config_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<UpdateAppConfigForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("apps.update") {
        return forbidden();
    }
    let input = UpdateAppConfigInput {
        app_id,
        compose_content: form.compose_content,
        env_content: form.env_content,
        binary_artifact_version: form.binary_artifact_version,
        binary_artifact_path: form.binary_artifact_path,
        binary_exec_args: form.binary_exec_args,
        binary_service_user: form.binary_service_user,
        binary_unit_name: form.binary_unit_name,
        binary_release_strategy: form.binary_release_strategy,
        binary_active_slot: form.binary_active_slot,
        binary_base_port: form.binary_base_port,
        binary_standby_port: form.binary_standby_port,
        binary_proxy_enabled: form.binary_proxy_enabled,
        binary_proxy_kind: form.binary_proxy_kind,
        binary_proxy_domain: form.binary_proxy_domain,
        binary_proxy_config_path: form.binary_proxy_config_path,
        health_check: match normalize_health_config(
            &form.health_check_kind,
            &form.health_endpoint,
            form.health_timeout_secs,
            form.health_expected_status,
        ) {
            Ok(config) => config,
            Err(err) => return (StatusCode::BAD_REQUEST, err.message().to_owned()).into_response(),
        },
    };
    match state.apps().update_app_config(input).await {
        Ok(()) => redirect(&format!("/apps/{app_id}")),
        Err(err) => app_error_response(err),
    }
}

async fn app_metadata_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_update_app_metadata_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if let Err(err) = normalize_deploy_strategy(&form.deploy_strategy) {
        return (StatusCode::BAD_REQUEST, err.message().to_owned()).into_response();
    }
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("apps.update") {
        return forbidden();
    }
    let input = UpdateAppMetadataInput {
        app_id,
        name: form.name,
        description: form.description,
        work_dir: form.work_dir,
        deploy_strategy: form.deploy_strategy,
        target_node_ids: form.target_node_ids,
    };
    match state.apps().update_app_metadata(input).await {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                "apps.update",
                "app",
                &app_id.to_string(),
                "更新应用基础信息和目标节点",
            )
            .await;
            redirect(&format!("/apps/{app_id}"))
        }
        Err(err) => app_error_response(err),
    }
}

async fn app_snapshot_restore_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((app_id, snapshot_id)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("apps.update") {
        return forbidden();
    }
    match state
        .apps()
        .restore_config_snapshot(app_id, snapshot_id)
        .await
    {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                "apps.snapshot_restore",
                "app",
                &app_id.to_string(),
                &format!("恢复配置快照 #{snapshot_id}"),
            )
            .await;
            redirect(&format!("/apps/{app_id}"))
        }
        Err(err) => app_error_response(err),
    }
}

async fn app_compose_config_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("services.deploy") {
        return forbidden();
    }
    let command = match state.apps().compose_config(app_id).await {
        Ok(output) => output,
        Err(err) => return app_error_response(err),
    };
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(err) => return app_error_response(err),
    };
    render_app_detail(&session, detail, Some(compose_result_view(command)), None)
}

async fn app_compose_logs_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_LOGS) {
        return forbidden();
    }
    let command = match state.apps().compose_logs(app_id).await {
        Ok(output) => output,
        Err(err) => return app_error_response(err),
    };
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(err) => return app_error_response(err),
    };
    render_app_detail(&session, detail, Some(compose_result_view(command)), None)
}

async fn app_compose_confirm_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((app_id, action)): Path<(i64, String)>,
) -> Response {
    let Some(action) = parse_compose_confirm_action(&action) else {
        return bad_request("不支持的 Compose 操作".to_owned());
    };
    render_deploy_confirm(
        &state,
        &session,
        app_id,
        DeployConfirmAction::Compose(action),
    )
    .await
}

async fn app_binary_confirm_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((app_id, action)): Path<(i64, String)>,
) -> Response {
    let Some(action) = parse_binary_confirm_action(&action) else {
        return bad_request("不支持的二进制操作".to_owned());
    };
    render_deploy_confirm(
        &state,
        &session,
        app_id,
        DeployConfirmAction::Binary(action),
    )
    .await
}

async fn app_compose_up_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<ConfirmTaskForm>,
) -> Response {
    app_compose_task_submit(state, session, app_id, form, ComposeTaskAction::Up).await
}

async fn app_compose_down_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<ConfirmTaskForm>,
) -> Response {
    app_compose_task_submit(state, session, app_id, form, ComposeTaskAction::Down).await
}

async fn app_compose_restart_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<ConfirmTaskForm>,
) -> Response {
    app_compose_task_submit(state, session, app_id, form, ComposeTaskAction::Restart).await
}

async fn app_binary_restart_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<ConfirmTaskForm>,
) -> Response {
    app_binary_task_submit(state, session, app_id, form, BinaryTaskAction::Restart).await
}

async fn app_binary_stop_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<ConfirmTaskForm>,
) -> Response {
    app_binary_task_submit(state, session, app_id, form, BinaryTaskAction::Stop).await
}

async fn app_binary_upload_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    multipart: Multipart,
) -> Response {
    if !session.can(ARTIFACTS_UPLOAD) {
        return forbidden();
    }
    let input = match parse_binary_upload_multipart(app_id, &session, multipart).await {
        Ok(input) => input,
        Err(response) => return response,
    };
    match state.apps().upload_binary_artifact(input).await {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                "artifacts.upload",
                "app",
                &app_id.to_string(),
                "上传二进制制品",
            )
            .await;
            redirect(&format!("/apps/{app_id}"))
        }
        Err(err) => app_error_response(err),
    }
}

async fn app_binary_release_activate_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((app_id, artifact_id)): Path<(i64, i64)>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("services.rollback") {
        return forbidden();
    }
    match state
        .apps()
        .rollback_binary_artifact(app_id, artifact_id, &session.account.username)
        .await
    {
        Ok(result) => {
            record_audit_event(
                &state,
                &session,
                "services.rollback",
                "task",
                &result.task_id.to_string(),
                &format!("回滚应用 #{app_id} 到二进制版本 {}", result.version),
            )
            .await;
            redirect(&format!("/tasks/{}", result.task_id))
        }
        Err(err) => app_error_response(err),
    }
}

#[derive(Clone, Copy)]
enum DeployConfirmAction {
    Compose(ComposeTaskAction),
    Binary(BinaryTaskAction),
}

async fn render_deploy_confirm(
    state: &AppState,
    session: &CurrentSession,
    app_id: i64,
    action: DeployConfirmAction,
) -> Response {
    if !session.can("services.deploy") {
        return forbidden();
    }
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(err) => return app_error_response(err),
    };
    if detail.app.status == "disabled" {
        return app_error_response(AppError::InvalidInput(
            "应用已停用，不能执行变更操作".to_owned(),
        ));
    }
    if detail.app.status == "deploying" {
        return app_error_response(AppError::Conflict(
            "应用正在部署中，请等待当前任务结束".to_owned(),
        ));
    }
    if detail.app.target_count <= 0 {
        return app_error_response(AppError::InvalidInput(
            "应用没有可用目标节点，请先启用节点或调整目标节点".to_owned(),
        ));
    }
    match action {
        DeployConfirmAction::Compose(_) if detail.app.app_type != "compose" => {
            return app_error_response(AppError::InvalidInput(
                "当前应用不是 Compose 应用".to_owned(),
            ));
        }
        DeployConfirmAction::Binary(_) if detail.app.app_type != "binary" => {
            return app_error_response(AppError::InvalidInput("当前应用不是二进制应用".to_owned()));
        }
        _ => {}
    }

    let nav_sections = nav_sections("/apps", session);
    let deploy_diff = deploy_diff_view(&detail.deploy_diff);
    let post_action = match action {
        DeployConfirmAction::Compose(action) => compose_submit_path(app_id, action),
        DeployConfirmAction::Binary(action) => binary_submit_path(app_id, action),
    };
    let action_label = deploy_confirm_action_label(action);
    let action_tone = deploy_confirm_action_tone(action);
    let action_description = deploy_confirm_action_description(action);
    let targets = detail.app.target_names.as_deref().unwrap_or("未绑定节点");
    let deploy_strategy = deploy_strategy_label(&detail.app.deploy_strategy);
    let health_endpoint = display_text(detail.health_check.endpoint.clone(), "无");
    let plan_node_order = detail
        .target_nodes
        .iter()
        .enumerate()
        .map(|(index, node)| format!("{}. {}", index + 1, node.name))
        .collect::<Vec<_>>()
        .join(" -> ");
    let plan_node_order = if plan_node_order.is_empty() {
        "未绑定节点".to_owned()
    } else {
        plan_node_order
    };
    let plan_failure_policy = deploy_plan_failure_policy(&detail.app.deploy_strategy);
    let deploy_plan_steps = deploy_plan_steps(&detail, action);
    let deploy_plan_files = deploy_plan_files(&detail, action);
    let preflight_rows = deploy_preflight_rows(&detail, action);
    let (preflight_summary, preflight_summary_tone) = deploy_preflight_summary(&preflight_rows);
    let (preflight_can_submit, preflight_submit_message) =
        deploy_preflight_submit_state(&preflight_rows);
    let can_manage_nodes = session.can(NODES_MANAGE);
    let can_install_nodes = session.can(NODES_INSTALL);
    let target_nodes = detail
        .target_nodes
        .iter()
        .map(|node| DeployConfirmTargetNodeRow {
            name: node.name.clone(),
            node_key: node.node_key.clone(),
            node_type: node_type_label(&node.node_type),
            status: node_status_label(&node.status),
            status_tone: node_status_tone(&node.status),
            docker_status: deploy_confirm_docker_status(&node.docker_status),
            preflight_hint: deploy_confirm_target_hint(&node.status, &node.docker_status),
        })
        .collect::<Vec<_>>();

    render_html(DeployConfirmTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        app_id: detail.app.id,
        app_name: &detail.app.name,
        app_key: &detail.app.app_key,
        app_type: app_type_label(&detail.app.app_type),
        work_dir: &detail.app.work_dir,
        action_label,
        action_tone,
        action_description,
        post_action,
        targets,
        target_count: detail.app.target_count,
        deploy_strategy,
        plan_node_order,
        plan_failure_policy,
        deploy_plan_steps: &deploy_plan_steps,
        deploy_plan_files: &deploy_plan_files,
        preflight_summary: &preflight_summary,
        preflight_summary_tone,
        preflight_rows: &preflight_rows,
        preflight_can_submit,
        preflight_submit_message,
        can_manage_nodes,
        can_install_nodes,
        target_nodes: &target_nodes,
        health_check_label: detail.health_check.kind.label(),
        health_endpoint: &health_endpoint,
        health_timeout_secs: detail.health_check.timeout_secs,
        health_expected_status: detail.health_check.expected_status,
        deploy_diff: &deploy_diff,
    })
}

async fn app_compose_task_submit(
    state: AppState,
    session: CurrentSession,
    app_id: i64,
    form: ConfirmTaskForm,
    action: ComposeTaskAction,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("services.deploy") {
        return forbidden();
    }
    if !form.is_confirmed() {
        return redirect(&compose_confirm_path(app_id, action));
    }
    match deploy_submit_blocker(&state, app_id, DeployConfirmAction::Compose(action)).await {
        Ok(Some(message)) => return app_error_response(AppError::InvalidInput(message)),
        Ok(None) => {}
        Err(err) => return app_error_response(err),
    }
    match state
        .apps()
        .run_compose_task(app_id, action, &session.account.username)
        .await
    {
        Ok(task_id) => {
            record_audit_event(
                &state,
                &session,
                compose_audit_action(action),
                "task",
                &task_id.to_string(),
                &format!("应用 #{app_id} 创建 Compose 任务"),
            )
            .await;
            redirect(&format!("/tasks/{task_id}"))
        }
        Err(err) => app_error_response(err),
    }
}

async fn parse_binary_upload_multipart(
    app_id: i64,
    session: &CurrentSession,
    mut multipart: Multipart,
) -> Result<UploadBinaryArtifactInput, Response> {
    let mut csrf_token = String::new();
    let mut artifact_version = String::new();
    let mut entry_file = String::new();
    let mut file_name = String::new();
    let mut bytes = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|err| {
        (StatusCode::BAD_REQUEST, format!("读取上传表单失败: {err}")).into_response()
    })? {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "csrf_token" => {
                csrf_token = field.text().await.map_err(|err| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("读取 CSRF 字段失败: {err}"),
                    )
                        .into_response()
                })?;
            }
            "artifact_version" => {
                artifact_version = field.text().await.map_err(|err| {
                    (StatusCode::BAD_REQUEST, format!("读取版本字段失败: {err}")).into_response()
                })?;
            }
            "entry_file" => {
                entry_file = field.text().await.map_err(|err| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("读取入口文件字段失败: {err}"),
                    )
                        .into_response()
                })?;
            }
            "artifact_file" => {
                file_name = field
                    .file_name()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| "artifact.bin".to_owned());
                bytes = field
                    .bytes()
                    .await
                    .map_err(|err| {
                        (StatusCode::BAD_REQUEST, format!("读取上传文件失败: {err}"))
                            .into_response()
                    })?
                    .to_vec();
            }
            _ => {}
        }
    }
    if !valid_csrf(session, &csrf_token) {
        return Err(forbidden());
    }
    Ok(UploadBinaryArtifactInput {
        app_id,
        artifact_version,
        file_name,
        bytes,
        entry_file,
    })
}

async fn app_binary_task_submit(
    state: AppState,
    session: CurrentSession,
    app_id: i64,
    form: ConfirmTaskForm,
    action: BinaryTaskAction,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("services.deploy") {
        return forbidden();
    }
    if !form.is_confirmed() {
        return redirect(&binary_confirm_path(app_id, action));
    }
    match deploy_submit_blocker(&state, app_id, DeployConfirmAction::Binary(action)).await {
        Ok(Some(message)) => return app_error_response(AppError::InvalidInput(message)),
        Ok(None) => {}
        Err(err) => return app_error_response(err),
    }
    match state
        .apps()
        .run_binary_task(app_id, action, &session.account.username)
        .await
    {
        Ok(task_id) => {
            record_audit_event(
                &state,
                &session,
                binary_audit_action(action),
                "task",
                &task_id.to_string(),
                &format!("应用 #{app_id} 创建二进制任务"),
            )
            .await;
            redirect(&format!("/tasks/{task_id}"))
        }
        Err(err) => app_error_response(err),
    }
}

async fn services_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ServicesQuery>,
) -> Response {
    if !session.can(SERVICES_VIEW) {
        return forbidden();
    }
    let services = match state.apps().list_services().await {
        Ok(services) => services,
        Err(err) => return app_error_response(err),
    };
    let selected_kind = normalize_service_kind_filter(query.kind.as_deref());
    let selected_status = normalize_service_status_filter(query.status.as_deref());
    let search_query = query.q.unwrap_or_default().trim().to_owned();
    let filtered_services = services
        .iter()
        .filter(|service| {
            service_matches_filters(service, selected_kind, selected_status, &search_query)
        })
        .collect::<Vec<_>>();
    let compose_count = services
        .iter()
        .filter(|service| service.service_kind == "Docker Compose")
        .count();
    let binary_count = services.len().saturating_sub(compose_count);
    let rows = filtered_services
        .iter()
        .map(|service| {
            let health_view =
                service_health_overview(&service.runtime_status, &service.health_check.kind);
            let node_links = service
                .target_nodes
                .iter()
                .map(|node| {
                    service_node_link_row(
                        node,
                        service_log_href(service.app_id, &service.service_name, node.id, 200),
                        "/services",
                        false,
                    )
                })
                .collect::<Vec<_>>();
            ServicePageRow {
                app_id: service.app_id,
                app_name: &service.app_name,
                app_key: &service.app_key,
                service_name: &service.service_name,
                service_kind: &service.service_kind,
                image: &service.image,
                ports: &service.ports,
                replicas: &service.replicas,
                targets: &service.target_names,
                app_status: app_status_label(&service.app_status),
                runtime_status: runtime_status_label(&service.runtime_status),
                runtime_status_tone: runtime_status_tone(&service.runtime_status),
                runtime_summary: &service.runtime_summary,
                active_version: task_display_text(&service.active_version, "未部署"),
                health_check: service.health_check.kind.label(),
                health_check_detail: service.health_check_detail.clone(),
                health_status: health_view.status,
                health_status_tone: health_view.tone,
                health_summary: health_view.summary,
                last_health_message: &service.last_health_message,
                last_health_at: &service.last_health_at,
                health_action_hint: health_view.action_hint,
                updated_at: &service.updated_at,
                node_links,
            }
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/apps", &session);
    render_html(ServicesTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        services: &rows,
        service_count: rows.len(),
        compose_count,
        binary_count,
        selected_kind,
        selected_status,
        query: &search_query,
        can_logs: session.can(SERVICES_LOGS),
        can_retry: session.can(TASKS_RETRY),
    })
}

async fn service_logs_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path((app_id, service_name)): Path<(i64, String)>,
    Query(query): Query<ServiceLogsQuery>,
) -> Response {
    if !session.can(SERVICES_LOGS) {
        return forbidden();
    }
    let tail_lines = normalize_log_tail_lines(query.tail);
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(err) => return app_error_response(err),
    };
    let log_output = if detail.app.app_type == "binary" {
        match state
            .apps()
            .binary_service_logs(app_id, &service_name, query.node_id, tail_lines)
            .await
        {
            Ok(output) => output,
            Err(err) => return app_error_response(err),
        }
    } else {
        match state
            .apps()
            .compose_service_logs(app_id, &service_name, query.node_id, tail_lines)
            .await
        {
            Ok(output) => output,
            Err(err) => return app_error_response(err),
        }
    };
    let command = compose_result_view(log_output.command_output);
    let selected_log_href = service_log_href(app_id, &service_name, log_output.node.id, tail_lines);
    let node_links = log_output
        .target_nodes
        .iter()
        .map(|node| {
            service_node_link_row(
                node,
                service_log_href(app_id, &service_name, node.id, tail_lines),
                &selected_log_href,
                node.id == log_output.node.id,
            )
        })
        .collect::<Vec<_>>();
    let tail_options =
        service_log_tail_options(app_id, &service_name, log_output.node.id, tail_lines);
    let node_runtime_summary = service_node_runtime_summary(&log_output.node);
    let node_last_health_at = service_node_last_health_at(&log_output.node);
    let selected_task_href = log_output
        .node
        .last_task_id
        .map(|task_id| task_href_with_return(task_id, &selected_log_href))
        .unwrap_or_default();
    let selected_task_id = log_output.node.last_task_id.unwrap_or_default();
    let has_selected_task = !selected_task_href.is_empty();
    let selected_task_action_label = service_task_action_label(&log_output.node);
    let selected_can_retry_task = service_node_can_retry_task(&log_output.node);
    let nav_sections = nav_sections("/apps", &session);
    render_html(ServiceLogsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        app_id,
        app_name: &detail.app.name,
        service_name: &service_name,
        node_name: &log_output.node.name,
        node_key: &log_output.node.node_key,
        selected_node_id: log_output.node.id,
        node_runtime_status: runtime_status_label(&log_output.node.runtime_status),
        node_runtime_status_tone: runtime_status_tone(&log_output.node.runtime_status),
        node_runtime_summary: &node_runtime_summary,
        node_active_version: task_display_text(&log_output.node.active_version, "未部署"),
        node_last_health_at: &node_last_health_at,
        node_last_message: &log_output.node.message,
        selected_task_href: &selected_task_href,
        selected_task_action_label,
        selected_task_id,
        selected_task_return_to: &selected_log_href,
        selected_can_retry_task,
        has_selected_task,
        node_links: &node_links,
        command: &command.command,
        status: command.status,
        status_tone: command.status_tone,
        status_code: &command.status_code,
        output: &command.output,
        tail_lines,
        tail_options: &tail_options,
    })
}

async fn nodes_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<NodesQuery>,
) -> Response {
    if !session.can(NODES_VIEW) {
        return forbidden();
    }
    let nodes = match state.nodes().list_nodes().await {
        Ok(nodes) => nodes,
        Err(err) => return node_error_response(err),
    };
    let selected_type = normalize_node_type_filter(query.r#type.as_deref());
    let selected_status = normalize_node_status_filter(query.status.as_deref());
    let search_query = query.q.unwrap_or_default().trim().to_owned();
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let credential_options = match state.node_credentials().active_options().await {
        Ok(options) => options
            .iter()
            .map(|option| NodeCredentialOptionRow {
                id: option.id,
                label: if option.fingerprint.is_empty() {
                    option.name.clone()
                } else {
                    format!("{} · {}", option.name, option.fingerprint)
                },
            })
            .collect::<Vec<_>>(),
        Err(err) => return node_credential_error_response(err),
    };
    let filtered_nodes = nodes
        .iter()
        .filter(|node| node_matches_filters(node, selected_type, selected_status, &search_query))
        .collect::<Vec<_>>();
    let can_manage_nodes = session.can(NODES_MANAGE);
    let can_install_nodes = session.can(NODES_INSTALL);
    let rows = filtered_nodes
        .iter()
        .map(|node| node_page_row(node, can_manage_nodes))
        .collect::<Vec<_>>();
    let mut node_details = Vec::with_capacity(filtered_nodes.len());
    for node in &filtered_nodes {
        let detail = match state.nodes().node_detail(node.id).await {
            Ok(detail) => detail,
            Err(err) => return node_error_response(err),
        };
        node_details.push(NodeDetailModalRow {
            capability_guides: node_capability_guides(&detail.node),
            checks: detail
                .checks
                .iter()
                .map(node_check_history_row)
                .collect::<Vec<_>>(),
            apps: detail
                .apps
                .iter()
                .map(node_app_runtime_row)
                .collect::<Vec<_>>(),
            tasks: detail.tasks.iter().map(node_task_row).collect::<Vec<_>>(),
            can_install: can_install_nodes,
        });
    }
    let nav_sections = nav_sections("/nodes", &session);
    render_html(NodesTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        nodes: &rows,
        node_details: &node_details,
        selected_type,
        selected_status,
        query: &search_query,
        default_node_work_dir: &platform_config.default_node_work_dir,
        credential_options: &credential_options,
        can_manage: can_manage_nodes,
    })
}

async fn node_detail_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(node_id): Path<i64>,
) -> Response {
    if !session.can(NODES_VIEW) {
        return forbidden();
    }
    let detail = match state.nodes().node_detail(node_id).await {
        Ok(detail) => detail,
        Err(err) => return node_error_response(err),
    };
    let node = &detail.node;
    let node_row = node_page_row(node, session.can(NODES_MANAGE));
    let checks = detail
        .checks
        .iter()
        .map(node_check_history_row)
        .collect::<Vec<_>>();
    let apps = detail
        .apps
        .iter()
        .map(node_app_runtime_row)
        .collect::<Vec<_>>();
    let tasks = detail.tasks.iter().map(node_task_row).collect::<Vec<_>>();
    let capability_guides = node_capability_guides(node);
    let nav_sections = nav_sections("/nodes", &session);
    render_html(NodeDetailTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        node: &node_row,
        capability_guides: &capability_guides,
        checks: &checks,
        apps: &apps,
        tasks: &tasks,
        can_manage: session.can(NODES_MANAGE),
        can_install: session.can(NODES_INSTALL),
    })
}

async fn create_node_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<CreateNodeForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(NODES_MANAGE) {
        return forbidden();
    }
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let work_dir = if form.work_dir.trim().is_empty() {
        platform_config.default_node_work_dir
    } else {
        form.work_dir
    };
    let input = CreateNodeInput {
        node_key: form.node_key,
        name: form.name,
        node_type: form.node_type,
        address: form.address,
        ssh_port: form.ssh_port,
        ssh_user: form.ssh_user,
        credential_id: form.credential_id,
        work_dir,
        region: form.region,
        labels: form.labels,
    };
    match state.nodes().create_node(input).await {
        Ok(()) => redirect("/nodes"),
        Err(err) => node_error_response(err),
    }
}

async fn node_update_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<UpdateNodeForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(NODES_MANAGE) {
        return forbidden();
    }
    let input = UpdateNodeInput {
        node_id: form.node_id,
        name: form.name,
        node_type: form.node_type,
        address: form.address,
        ssh_port: form.ssh_port,
        ssh_user: form.ssh_user,
        credential_id: form.credential_id,
        work_dir: form.work_dir,
        region: form.region,
        labels: form.labels,
    };
    match state.nodes().update_node(input).await {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                NODES_MANAGE,
                "node",
                &form.node_id.to_string(),
                "更新节点配置",
            )
            .await;
            redirect("/nodes")
        }
        Err(err) => node_error_response(err),
    }
}

async fn node_status_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<NodeStatusForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(NODES_MANAGE) {
        return forbidden();
    }
    match state
        .nodes()
        .set_node_status(form.node_id, &form.status)
        .await
    {
        Ok(change) => {
            record_audit_event(
                &state,
                &session,
                "nodes.status",
                "node",
                &change.node_id.to_string(),
                &format!(
                    "{} 状态 {} -> {}",
                    change.node_name,
                    node_status_label(&change.previous_status),
                    node_status_label(&change.status)
                ),
            )
            .await;
            redirect("/nodes")
        }
        Err(err) => node_error_response(err),
    }
}

async fn node_check_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    headers: HeaderMap,
    Form(form): Form<NodeCheckForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(NODES_MANAGE) {
        return forbidden();
    }
    let ajax_request = is_node_check_ajax_request(&headers);
    match state.nodes().check_node(form.node_id).await {
        Ok(result) if ajax_request => Json(NodeCheckAjaxResponse {
            status: node_check_result_node_status_label(&result.status),
            status_tone: node_check_result_node_status_tone(&result.status),
            docker_status: node_check_result_docker_status(&result),
            message: &result.message,
        })
        .into_response(),
        Ok(_) => redirect(node_check_return_path(form.return_to.as_deref())),
        Err(err) => node_error_response(err),
    }
}

async fn node_install_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<NodeInstallForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(NODES_INSTALL) {
        return forbidden();
    }
    let component = match NodeInstallComponent::parse(&form.component) {
        Ok(component) => component,
        Err(err) => return node_error_response(err),
    };
    match state
        .nodes()
        .create_install_task(
            state.tasks(),
            form.node_id,
            component,
            &session.account.username,
        )
        .await
    {
        Ok(result) => {
            record_audit_event(
                &state,
                &session,
                "nodes.install",
                "node",
                &form.node_id.to_string(),
                &format!(
                    "节点 {} 创建 {} 安装任务 #{}",
                    result.node_name,
                    result.component.label(),
                    result.task_id
                ),
            )
            .await;
            redirect(&task_detail_redirect_path(
                result.task_id,
                form.return_to.as_deref(),
            ))
        }
        Err(err) => node_error_response(err),
    }
}

async fn node_credentials_page(State(state): State<AppState>, session: CurrentSession) -> Response {
    if !session.can(crate::auth::NODE_CREDENTIALS_VIEW) {
        return forbidden();
    }
    let credentials = match state.node_credentials().list_credentials().await {
        Ok(credentials) => credentials,
        Err(err) => return node_credential_error_response(err),
    };
    let rows = credentials
        .iter()
        .map(|credential| {
            let status = credential_status_label(&credential.status);
            NodeCredentialPageRow {
                id: credential.id,
                name: &credential.name,
                credential_key: &credential.credential_key,
                public_key: &credential.public_key,
                fingerprint: &credential.fingerprint,
                private_key_path: &credential.private_key_path,
                passphrase_hint: if credential.passphrase_hint.is_empty() {
                    "无"
                } else {
                    &credential.passphrase_hint
                },
                status,
                status_tone: credential_status_tone(&credential.status),
                created_by: if credential.created_by.is_empty() {
                    "system"
                } else {
                    &credential.created_by
                },
                created_at: &credential.created_at,
                updated_at: &credential.updated_at,
                bound_node_count: credential.bound_node_count,
                toggle_status: credential_status_toggle_value(&credential.status),
                toggle_label: credential_status_toggle_label(&credential.status),
            }
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/node-credentials", &session);
    render_html(NodeCredentialsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        credentials: &rows,
        can_manage: session.can(crate::auth::NODE_CREDENTIALS_MANAGE),
    })
}

async fn node_credential_generate_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<GenerateNodeCredentialForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(crate::auth::NODE_CREDENTIALS_MANAGE) {
        return forbidden();
    }
    match state
        .node_credentials()
        .create_generated_key(CreateGeneratedCredentialInput {
            name: form.name,
            key_algorithm: form.key_algorithm,
            created_by: session.account.username.clone(),
        })
        .await
    {
        Ok(created) => {
            record_audit_event(
                &state,
                &session,
                "node_credentials.generate",
                "node_credential",
                &created.id.to_string(),
                &format!("生成节点凭据 {} ({})", created.name, created.fingerprint),
            )
            .await;
            redirect("/node-credentials")
        }
        Err(err) => node_credential_error_response(err),
    }
}

async fn node_credential_upload_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<UploadNodeCredentialForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(crate::auth::NODE_CREDENTIALS_MANAGE) {
        return forbidden();
    }
    match state
        .node_credentials()
        .create_uploaded_key(CreateUploadedCredentialInput {
            name: form.name,
            private_key: form.private_key,
            public_key: form.public_key,
            passphrase_hint: form.passphrase_hint,
            created_by: session.account.username.clone(),
        })
        .await
    {
        Ok(created) => {
            record_audit_event(
                &state,
                &session,
                "node_credentials.upload",
                "node_credential",
                &created.id.to_string(),
                &format!("录入节点凭据 {} ({})", created.name, created.fingerprint),
            )
            .await;
            redirect("/node-credentials")
        }
        Err(err) => node_credential_error_response(err),
    }
}

async fn node_credential_status_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<NodeCredentialStatusForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(crate::auth::NODE_CREDENTIALS_MANAGE) {
        return forbidden();
    }
    match state
        .node_credentials()
        .set_status(form.credential_id, &form.status)
        .await
    {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                "node_credentials.status",
                "node_credential",
                &form.credential_id.to_string(),
                &format!(
                    "更新节点凭据状态为 {}",
                    credential_status_label(&form.status)
                ),
            )
            .await;
            redirect("/node-credentials")
        }
        Err(err) => node_credential_error_response(err),
    }
}

async fn tasks_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<TaskListQuery>,
) -> Response {
    if !session.can(TASKS_VIEW) {
        return forbidden();
    }
    let filter = TaskListFilter {
        status: query.status.clone(),
        phase: query.phase.clone(),
        app_id: query.app_id,
        task_kind: query.task_kind.clone(),
        query: query.q.clone(),
    };
    let tasks = match state.tasks().list_tasks_filtered(filter.clone()).await {
        Ok(tasks) => tasks,
        Err(err) => return task_error_response(err),
    };
    let status_counts = match state.tasks().task_status_counts(filter.clone()).await {
        Ok(counts) => counts,
        Err(err) => return task_error_response(err),
    };
    let apps = match state.apps().list_apps().await {
        Ok(apps) => apps,
        Err(err) => return app_error_response(err),
    };
    let queue_summary = task_queue_summary(&tasks);
    let rows = tasks
        .iter()
        .map(|task| {
            let queue_state = task_queue_state(task.status.as_str(), task.id, &tasks);
            TaskPageRow {
                id: task.id,
                title: &task.title,
                task_kind_label: task_kind_label(&task.task_kind),
                app_name: task.app_name.as_deref().unwrap_or("未关联应用"),
                status: task_status_label(&task.status),
                status_tone: task_status_tone(&task.status),
                phase: task_phase_label(&task.phase),
                phase_tone: task_phase_tone(&task.phase),
                queue_tone: queue_state.tone,
                queue_state: queue_state.label,
                command: if task.command.is_empty() {
                    "尚未执行"
                } else {
                    &task.command
                },
                summary: if task.summary.is_empty() {
                    "暂无输出"
                } else {
                    &task.summary
                },
                exit_code: task
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "无".to_owned()),
                created_by: &task.created_by,
                created_at: &task.created_at,
                updated_at: &task.updated_at,
            }
        })
        .collect::<Vec<_>>();
    let selected_status = query.status.as_deref().unwrap_or_default();
    let selected_phase = query.phase.as_deref().unwrap_or_default();
    let selected_app_id = query
        .app_id
        .map(|app_id| app_id.to_string())
        .unwrap_or_default();
    let selected_task_kind = query.task_kind.as_deref().unwrap_or_default();
    let query_text = query.q.as_deref().unwrap_or_default();
    let status_filters = task_status_filter_rows(selected_status, &status_counts);
    let phase_filters = task_phase_filter_rows(selected_phase);
    let kind_filters = task_kind_filter_rows(selected_task_kind);
    let app_filters = apps
        .iter()
        .map(|app| TaskAppFilterRow {
            id: app.id,
            name: &app.name,
            selected: Some(app.id) == query.app_id,
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/tasks", &session);
    render_html(TasksTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        tasks: &rows,
        status_filters: &status_filters,
        phase_filters: &phase_filters,
        app_filters: &app_filters,
        kind_filters: &kind_filters,
        queue_summary: &queue_summary,
        selected_app_id: &selected_app_id,
        query: query_text,
        filtered_count: rows.len(),
    })
}

async fn task_detail_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(task_id): Path<i64>,
    Query(query): Query<TaskDetailQuery>,
) -> Response {
    if !session.can(TASKS_VIEW) {
        return forbidden();
    }
    let task = match state.tasks().task_detail(task_id).await {
        Ok(task) => task,
        Err(err) => return task_error_response(err),
    };
    let logs = match state.tasks().task_logs(task_id).await {
        Ok(logs) => logs,
        Err(err) => return task_error_response(err),
    };
    let node_results = match state.tasks().task_node_results(task_id).await {
        Ok(node_results) => node_results,
        Err(err) => return task_error_response(err),
    };
    let queue_position = match state.tasks().task_queue_position(task_id).await {
        Ok(queue_position) => queue_position,
        Err(err) => return task_error_response(err),
    };
    let queue_state = task_detail_queue_state(&task.status, &queue_position);
    let execution_guide = task_execution_guide_view(&task, &node_results);
    let task_view = TaskDetailView {
        id: task.id,
        title: &task.title,
        task_kind_label: task_kind_label(&task.task_kind),
        app_name: task.app_name.as_deref().unwrap_or("未关联应用"),
        node_name: task.node_name.as_deref().unwrap_or("未绑定节点"),
        status: task_status_label(&task.status),
        status_tone: task_status_tone(&task.status),
        phase: task_phase_label(&task.phase),
        phase_tone: task_phase_tone(&task.phase),
        phase_detail: task_phase_detail(&task.phase),
        queue_tone: queue_state.tone,
        queue_state: queue_state.label,
        command: task_display_text(&task.command, "尚未执行"),
        summary: task_display_text(&task.summary, "暂无输出"),
        exit_code: task
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "无".to_owned()),
        created_by: &task.created_by,
        started_at: task.started_at.as_deref().unwrap_or("未开始"),
        finished_at: task.finished_at.as_deref().unwrap_or("未结束"),
        created_at: &task.created_at,
        updated_at: &task.updated_at,
        is_failed: task.status == "failed",
        is_queued: task.status == "queued",
        is_live: matches!(task.status.as_str(), "queued" | "running"),
        is_retryable_task: is_retryable_task_kind(&task.task_kind),
    };
    let phase_rows = task_phase_step_rows(&task.phase);
    let node_result_rows = node_results
        .iter()
        .map(|result| {
            let action = task_node_result_action(result);
            TaskNodeResultRow {
                node_id: result.node_id,
                node_name: &result.node_name,
                node_key: &result.node_key,
                node_type: &result.node_type,
                status: task_node_result_status_label(&result.status),
                status_tone: task_node_result_status_tone(&result.status),
                message: &result.message,
                command_count: result.command_count,
                finished_at: &result.finished_at,
                action_kind: action.kind,
                action_label: action.label,
                action_component: action.component,
                action_hint: action.hint,
                has_action: action.has_action(),
            }
        })
        .collect::<Vec<_>>();
    let log_rows = logs
        .iter()
        .map(|log| TaskLogRow {
            id: log.id,
            stream: &log.stream,
            stream_tone: task_log_stream_tone(&log.stream),
            content: &log.content,
            created_at: &log.created_at,
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/tasks", &session);
    let return_to = task_return_path(query.return_to.as_deref()).map(str::to_owned);
    let return_action = task_return_action_view(return_to.as_deref());
    let install_check_node_id =
        if return_action.has_return && is_node_install_task_kind(&task.task_kind) {
            task.node_id
        } else {
            None
        };
    render_html(TaskDetailTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        task: task_view,
        execution_guide,
        return_action,
        phases: &phase_rows,
        node_results: &node_result_rows,
        logs: &log_rows,
        can_retry: session.can(TASKS_RETRY),
        can_cancel: session.can("tasks.cancel"),
        can_check_node: session.can(NODES_MANAGE),
        can_install_nodes: session.can(NODES_INSTALL),
        install_check_node_id,
    })
}

async fn task_cancel_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(task_id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("tasks.cancel") {
        return forbidden();
    }
    match state
        .tasks()
        .cancel_queued(task_id, &session.account.username)
        .await
    {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                "tasks.cancel",
                "task",
                &task_id.to_string(),
                "取消等待中的任务",
            )
            .await;
            redirect(&format!("/tasks/{task_id}"))
        }
        Err(err) => task_error_response(err),
    }
}

async fn task_retry_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(task_id): Path<i64>,
    Form(form): Form<TaskRetryForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(TASKS_RETRY) {
        return forbidden();
    }
    let result = match state.tasks().task_detail(task_id).await {
        Ok(task) if is_compose_task_kind(&task.task_kind) => {
            state
                .apps()
                .retry_compose_task(task_id, &session.account.username)
                .await
        }
        Ok(task) if is_binary_task_kind(&task.task_kind) => {
            state
                .apps()
                .retry_binary_task(task_id, &session.account.username)
                .await
        }
        Ok(_) => Err(AppError::InvalidInput("当前任务不支持重试".to_owned())),
        Err(err) => return task_error_response(err),
    };
    match result {
        Ok(new_task_id) => {
            record_audit_event(
                &state,
                &session,
                "tasks.retry",
                "task",
                &new_task_id.to_string(),
                &format!("从任务 #{task_id} 重试创建新任务"),
            )
            .await;
            redirect(&task_detail_redirect_path(
                new_task_id,
                form.return_to.as_deref(),
            ))
        }
        Err(err) => app_error_response(err),
    }
}

async fn templates_page(State(state): State<AppState>, session: CurrentSession) -> Response {
    if !session.can(TEMPLATES_VIEW) {
        return forbidden();
    }
    let node_options = match state.apps().node_options().await {
        Ok(nodes) => nodes,
        Err(err) => return app_error_response(err),
    };
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let template_rows = compose_templates()
        .iter()
        .map(|template| TemplateCardRow {
            key: template.key,
            name: template.name,
            description: template.description,
            image: template.image,
            default_port: template.default_port,
            env_hint: template.env_hint,
        })
        .collect::<Vec<_>>();
    let node_choices = node_options
        .iter()
        .map(|node| AppNodeChoiceRow {
            id: node.id,
            label: &node.name,
            detail: &node.node_key,
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/templates", &session);
    render_html(TemplatesTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        templates: &template_rows,
        node_choices: &node_choices,
        default_app_work_dir: &platform_config.default_app_work_dir_for("redis-cache"),
        default_app_work_dir_template: &platform_config.default_app_work_dir,
        default_template_port: template_rows
            .first()
            .map(|template| template.default_port)
            .unwrap_or(8080),
        can_create: session.can("apps.create"),
    })
}

async fn create_template_app_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_create_template_app_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if let Err(err) = normalize_deploy_strategy(&form.deploy_strategy) {
        return (StatusCode::BAD_REQUEST, err.message().to_owned()).into_response();
    }
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("apps.create") {
        return forbidden();
    }
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let work_dir = if form.work_dir.trim().is_empty() {
        platform_config.default_app_work_dir_for(&form.app_key)
    } else {
        form.work_dir
    };
    let rendered = match render_compose_template(RenderTemplateInput {
        template_key: &form.template_key,
        app_key: &form.app_key,
        port: form.port,
    }) {
        Ok(rendered) => rendered,
        Err(err) => return (StatusCode::BAD_REQUEST, err.message().to_owned()).into_response(),
    };
    let input = CreateAppInput {
        app_key: form.app_key,
        name: form.name,
        description: form.description,
        app_type: "compose".to_owned(),
        deploy_strategy: form.deploy_strategy,
        work_dir,
        compose_content: rendered.compose_content,
        env_content: rendered.env_content,
        binary_artifact_version: String::new(),
        binary_artifact_path: String::new(),
        binary_exec_args: String::new(),
        binary_service_user: String::new(),
        binary_unit_name: String::new(),
        binary_release_strategy: String::new(),
        binary_active_slot: String::new(),
        binary_base_port: 0,
        binary_standby_port: 0,
        binary_proxy_enabled: false,
        binary_proxy_kind: String::new(),
        binary_proxy_domain: String::new(),
        binary_proxy_config_path: String::new(),
        target_node_ids: form.target_node_ids,
    };
    match state.apps().create_app(input).await {
        Ok(app_id) => redirect(&format!("/apps/{app_id}?notice=created")),
        Err(err) => app_error_response(err),
    }
}

async fn artifacts_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ArtifactsQuery>,
) -> Response {
    if !session.can(ARTIFACTS_VIEW) {
        return forbidden();
    }
    let artifacts = match state.apps().list_artifacts().await {
        Ok(artifacts) => artifacts,
        Err(err) => return app_error_response(err),
    };
    let active_count = artifacts
        .iter()
        .filter(|artifact| artifact.status == "active")
        .count();
    let uploaded_count = artifacts
        .iter()
        .filter(|artifact| artifact.metadata_value("source") == "upload")
        .count();
    let registered_count = artifacts.len().saturating_sub(uploaded_count);
    let latest_time = artifacts
        .first()
        .map(|artifact| artifact.created_at.clone())
        .unwrap_or_else(|| "暂无制品".to_owned());
    let selected_status = normalize_artifact_status_filter(query.status.as_deref());
    let selected_kind = normalize_artifact_kind_filter(query.kind.as_deref());
    let selected_source = normalize_artifact_source_filter(query.source.as_deref());
    let search_query = query.q.unwrap_or_default().trim().to_owned();
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let filtered_artifacts = artifacts
        .iter()
        .filter(|artifact| {
            artifact_matches_filters(
                artifact,
                selected_status,
                selected_kind,
                selected_source,
                &search_query,
            )
        })
        .collect::<Vec<_>>();
    let rows = filtered_artifacts
        .iter()
        .map(|artifact| ArtifactPageRow {
            id: artifact.id,
            app_id: artifact.app_id,
            app_name: artifact.app_name.clone(),
            app_key: artifact.app_key.clone(),
            version: artifact.version.clone(),
            artifact_kind: artifact_kind_label(&artifact.artifact_kind).to_owned(),
            status: artifact_status_label(&artifact.status),
            status_tone: artifact_status_tone(&artifact.status),
            artifact_path: artifact.artifact_path.clone(),
            sha256: short_hash(&artifact.metadata_value("sha256")),
            size: format_size(&artifact.metadata_value("size_bytes")),
            entry_file: display_text(artifact.metadata_value("entry_file"), "未记录"),
            source: artifact_source_label(&artifact.metadata_value("source")).to_owned(),
            created_at: artifact.created_at.clone(),
        })
        .collect::<Vec<_>>();
    let summary_items = vec![
        SummaryItem {
            label: "制品总数",
            value: artifacts.len().to_string(),
            detail: "最近 100 条二进制制品".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "当前版本",
            value: active_count.to_string(),
            detail: "每个二进制应用最多一个当前版本".to_owned(),
            tone: "success",
        },
        SummaryItem {
            label: "上传制品",
            value: uploaded_count.to_string(),
            detail: format!("登记路径 {registered_count} 个"),
            tone: "active",
        },
        SummaryItem {
            label: "最近更新",
            value: latest_time,
            detail: "按创建时间倒序展示".to_owned(),
            tone: "neutral",
        },
    ];
    let nav_sections = nav_sections("/artifacts", &session);
    render_html(ArtifactsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        artifacts: &rows,
        selected_status,
        selected_kind,
        selected_source,
        query: &search_query,
        uploaded_binary_releases_to_keep: platform_config.uploaded_binary_releases_to_keep,
    })
}

async fn settings_page(State(state): State<AppState>, session: CurrentSession) -> Response {
    if !session.can(SETTINGS_VIEW) {
        return forbidden();
    }
    let settings = state.settings();
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let data_dir = settings.data_dir.to_string_lossy().to_string();
    let apps_dir = settings.data_dir.join("apps").to_string_lossy().to_string();
    let database_kind = if settings.database_url.starts_with("sqlite:") {
        "SQLite"
    } else {
        "外部数据库"
    };
    let summary_items = vec![
        SummaryItem {
            label: "监听地址",
            value: settings.bind.to_string(),
            detail: "API 与后台页面服务地址".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "数据目录",
            value: data_dir.clone(),
            detail: "应用运行文件、release 和 current 指针根目录".to_owned(),
            tone: "active",
        },
        SummaryItem {
            label: "数据库",
            value: database_kind.to_owned(),
            detail: "保存账号、节点、应用、任务和审计索引".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "会话存储",
            value: "内存".to_owned(),
            detail: "Access/Refresh Token 的服务端会话索引，服务重启后需要重新登录。".to_owned(),
            tone: "active",
        },
    ];
    let runtime_rows = vec![
        SettingsRow {
            label: "服务绑定",
            value: settings.bind.to_string(),
            detail: "来自 EASY_DEPLOY_BIND 或 --bind。",
        },
        SettingsRow {
            label: "面板版本",
            value: concat!("v", env!("CARGO_PKG_VERSION")).to_owned(),
            detail: "当前 api 模块版本。",
        },
        SettingsRow {
            label: "资源版本",
            value: ASSET_VERSION.to_owned(),
            detail: "用于刷新 CSS、logo 和 favicon 缓存。",
        },
    ];
    let storage_rows = vec![
        SettingsRow {
            label: "数据目录",
            value: data_dir,
            detail: "来自 EASY_DEPLOY_DATA_DIR 或 --data-dir。",
        },
        SettingsRow {
            label: "应用目录",
            value: apps_dir,
            detail: "每个应用会在此目录下生成 compose.yaml、.env、release 等文件。",
        },
        SettingsRow {
            label: "数据库地址",
            value: settings.database_url.clone(),
            detail: "来自 EASY_DEPLOY_DATABASE_URL 或 --database-url。",
        },
    ];
    let auth_rows = vec![
        SettingsRow {
            label: "会话存储",
            value: "内存".to_owned(),
            detail: "部署平台不依赖 Redis；服务重启会清空登录态，需要重新登录。",
        },
        SettingsRow {
            label: "授权方案",
            value: "HttpOnly Cookie + Access/Refresh 双 Token".to_owned(),
            detail: "Refresh Token 会轮换，会话可在后台强制下线。",
        },
        SettingsRow {
            label: "Cookie Secure",
            value: if settings.cookie_secure {
                "已启用 Secure".to_owned()
            } else {
                "未启用 Secure".to_owned()
            },
            detail: "来自 EASY_DEPLOY_COOKIE_SECURE 或 --cookie-secure，生产 HTTPS 建议启用。",
        },
    ];
    let template_port_summary = compose_templates()
        .iter()
        .map(|template| format!("{} {}", template.key, template.default_port))
        .collect::<Vec<_>>()
        .join(" / ");
    let deploy_rows = vec![
        SettingsRow {
            label: "模板默认端口",
            value: template_port_summary,
            detail: "内置 Compose 模板创建应用时的默认主机端口，可在创建表单里覆盖。",
        },
        SettingsRow {
            label: "默认节点目录",
            value: platform_config.default_node_work_dir.clone(),
            detail: "新增节点时的默认工作目录，可在本页保存后立即用于新建表单。",
        },
        SettingsRow {
            label: "健康检查超时",
            value: "5 秒".to_owned(),
            detail: "应用未配置时使用 none；HTTP/TCP/systemd 检查默认 5 秒。",
        },
        SettingsRow {
            label: "命令执行超时",
            value: format!("{} 秒", settings.command_timeout_secs.max(1)),
            detail: "来自 EASY_DEPLOY_COMMAND_TIMEOUT_SECS 或 --command-timeout-secs，作用于 Docker、systemd、SSH 和 scp 命令。",
        },
        SettingsRow {
            label: "上传制品保留",
            value: format!(
                "最近 {} 个版本",
                platform_config.uploaded_binary_releases_to_keep
            ),
            detail: "上传二进制制品后的保留数量，当前版本永远不会被清理。",
        },
        SettingsRow {
            label: "任务队列",
            value: "进程内 Tokio 队列".to_owned(),
            detail: "先保持单体易部署，后续再接外部 worker。",
        },
    ];
    let nav_sections = nav_sections("/settings", &session);
    render_html(SettingsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        runtime_rows: &runtime_rows,
        storage_rows: &storage_rows,
        auth_rows: &auth_rows,
        deploy_rows: &deploy_rows,
        can_update: session.can(SETTINGS_UPDATE),
        default_app_work_dir: &platform_config.default_app_work_dir,
        default_node_work_dir: &platform_config.default_node_work_dir,
        uploaded_binary_releases_to_keep: platform_config.uploaded_binary_releases_to_keep,
    })
}

async fn settings_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<SettingsForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SETTINGS_UPDATE) {
        return forbidden();
    }
    match state
        .platform()
        .update_config(
            UpdatePlatformConfigInput {
                default_app_work_dir: form.default_app_work_dir,
                default_node_work_dir: form.default_node_work_dir,
                uploaded_binary_releases_to_keep: form.uploaded_binary_releases_to_keep,
            },
            &session.account.username,
        )
        .await
    {
        Ok(config) => {
            record_audit_event(
                &state,
                &session,
                SETTINGS_UPDATE,
                "settings",
                "platform",
                &format!(
                    "更新平台设置：应用目录模板 {}，节点目录 {}，上传制品保留 {} 个版本",
                    config.default_app_work_dir,
                    config.default_node_work_dir,
                    config.uploaded_binary_releases_to_keep
                ),
            )
            .await;
            redirect("/settings")
        }
        Err(err) => platform_error_response(err),
    }
}

async fn audit_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<AuditLogQuery>,
) -> Response {
    if !session.can(AUDIT_VIEW) {
        return forbidden();
    }
    let filter = AuditLogFilter {
        action: query.action.clone(),
        target_type: query.target_type.clone(),
        actor: query.actor.clone(),
        query: query.q.clone(),
    };
    let logs = match state.auth().list_audit_logs_filtered(filter).await {
        Ok(logs) => logs,
        Err(err) => return err.into_response(),
    };
    let action_options = match state.auth().audit_action_options().await {
        Ok(options) => options,
        Err(err) => return err.into_response(),
    };
    let target_options = match state.auth().audit_target_type_options().await {
        Ok(options) => options,
        Err(err) => return err.into_response(),
    };
    let rows = logs
        .iter()
        .map(|log| AuditLogRow {
            actor: if log.actor_username.is_empty() {
                "系统"
            } else {
                &log.actor_username
            },
            action: &log.action,
            target: &log.target_type,
            message: &log.message,
            ip: &log.ip,
            created_at: &log.created_at,
        })
        .collect::<Vec<_>>();
    let selected_action = query.action.as_deref().unwrap_or_default();
    let selected_target_type = query.target_type.as_deref().unwrap_or_default();
    let actor = query.actor.as_deref().unwrap_or_default();
    let query_text = query.q.as_deref().unwrap_or_default();
    let action_filters = action_options
        .into_iter()
        .map(|option| AuditFilterOptionRow {
            selected: option.value == selected_action,
            value: option.value,
        })
        .collect::<Vec<_>>();
    let target_filters = target_options
        .into_iter()
        .map(|option| AuditFilterOptionRow {
            selected: option.value == selected_target_type,
            value: option.value,
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/audit", &session);
    render_html(AuditTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        logs: &rows,
        action_filters: &action_filters,
        target_filters: &target_filters,
        selected_action,
        selected_target_type,
        actor,
        query: query_text,
        filtered_count: rows.len(),
    })
}

async fn accounts_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<AccountsQuery>,
) -> Response {
    if !session.can(RBAC_ACCOUNTS_VIEW) {
        return forbidden();
    }
    let role_options = match state.auth().list_role_options().await {
        Ok(roles) => roles,
        Err(err) => return err.into_response(),
    };
    let accounts = match state.auth().list_accounts().await {
        Ok(accounts) => accounts,
        Err(err) => return err.into_response(),
    };
    let role_options_rows = role_options
        .iter()
        .map(|role| templates::RoleOptionRow {
            id: role.id,
            name: &role.role_name,
            code: &role.role_code,
        })
        .collect::<Vec<_>>();
    let selected_status = normalize_rbac_filter(query.status.as_deref());
    let selected_role = normalize_rbac_filter(query.role.as_deref());
    let query_text = query.q.as_deref().unwrap_or_default().trim();
    let notice = account_notice_message(query.notice.as_deref());
    let summary_items = account_summary_items(&accounts);
    let status_filters = account_status_filter_rows(selected_status);
    let role_filters = account_role_filter_rows(&role_options, selected_role);
    let rows = accounts
        .iter()
        .filter(|account| {
            account_matches_filter(account, selected_status, selected_role, query_text)
        })
        .map(|account| {
            let assigned_role_ids = parse_id_csv(account.role_ids.as_deref());
            let (security, security_tone) = account_security_view(account);
            AccountRow {
                id: account.id,
                is_current: account.id == session.account.id,
                username: &account.username,
                display_name: &account.display_name,
                roles: account.role_names.as_deref().unwrap_or("未分配"),
                status: account_status_label(&account.status),
                status_tone: account_status_tone(&account.status),
                security,
                security_tone,
                active_session_count: account.active_session_count,
                last_login_at: account.last_login_at.as_deref().unwrap_or("未登录"),
                toggle_label: account_status_toggle_label(&account.status),
                toggle_status: account_status_toggle_value(&account.status),
                role_choices: role_options
                    .iter()
                    .map(|role| templates::RoleChoiceRow {
                        id: role.id,
                        name: &role.role_name,
                        checked: assigned_role_ids.contains(&role.id),
                    })
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/admin/accounts", &session);
    let can_manage = session.can("rbac.accounts.manage");
    render_html(AccountsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        accounts: &rows,
        role_options: &role_options_rows,
        status_filters: &status_filters,
        role_filters: &role_filters,
        query: query_text,
        filtered_count: rows.len(),
        can_manage,
        notice,
    })
}

async fn roles_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<RolesQuery>,
) -> Response {
    if !session.can(RBAC_ROLES_VIEW) {
        return forbidden();
    }
    let roles = match state.auth().list_roles().await {
        Ok(roles) => roles,
        Err(err) => return err.into_response(),
    };
    let permission_groups = match state.auth().permission_groups().await {
        Ok(groups) => groups,
        Err(err) => return err.into_response(),
    };
    let permission_groups = permission_group_rows(&permission_groups);
    let selected_status = normalize_rbac_filter(query.status.as_deref());
    let selected_module = normalize_rbac_filter(query.module.as_deref());
    let query_text = query.q.as_deref().unwrap_or_default().trim();
    let total_permission_count = permission_groups
        .iter()
        .map(|group| group.permissions.len())
        .sum::<usize>();
    let summary_items = role_summary_items(&roles, total_permission_count);
    let status_filters = role_status_filter_rows(selected_status);
    let module_filters = role_module_filter_rows(&permission_groups, selected_module);
    let permission_dependencies_json = permission_dependencies_json(&permission_groups);
    let role_rows = roles
        .iter()
        .filter(|role| {
            role_matches_filter(
                role,
                &permission_groups,
                selected_status,
                selected_module,
                query_text,
            )
        })
        .map(|role| {
            let assigned_permission_ids = parse_id_csv(role.permission_ids.as_deref());
            let action_permission_count =
                role_action_permission_count(&permission_groups, &assigned_permission_ids);
            let coverage_percent =
                permission_coverage_percent(assigned_permission_ids.len(), total_permission_count);
            RoleRow {
                id: role.id,
                is_system: role.is_system == 1,
                role_code: &role.role_code,
                role_name: &role.role_name,
                description: &role.description,
                status: if role.status == "active" {
                    "启用"
                } else {
                    "禁用"
                },
                status_tone: if role.status == "active" {
                    "success"
                } else {
                    "warning"
                },
                permission_count: role.permission_count,
                action_permission_count,
                coverage_percent,
                system_label: if role.is_system == 1 {
                    "系统内置"
                } else {
                    "自定义"
                },
                toggle_label: if role.status == "active" {
                    "禁用"
                } else {
                    "启用"
                },
                permission_groups: permission_groups
                    .iter()
                    .map(|group| templates::PermissionChoiceGroup {
                        module: group.module,
                        permissions: group
                            .permissions
                            .iter()
                            .map(|permission| templates::PermissionChoiceRow {
                                id: permission.id,
                                name: permission.name,
                                key: permission.key,
                                checked: assigned_permission_ids.contains(&permission.id),
                            })
                            .collect(),
                    })
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/admin/roles", &session);
    let can_manage = session.can("rbac.roles.manage");
    render_html(RolesTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        roles: &role_rows,
        permission_groups: &permission_groups,
        status_filters: &status_filters,
        module_filters: &module_filters,
        permission_dependencies_json: &permission_dependencies_json,
        query: query_text,
        filtered_count: role_rows.len(),
        can_manage,
    })
}

async fn permissions_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<PermissionsQuery>,
) -> Response {
    if !session.can(RBAC_PERMISSIONS_VIEW) {
        return forbidden();
    }
    let permission_groups = match state.auth().permission_groups().await {
        Ok(groups) => groups,
        Err(err) => return err.into_response(),
    };
    let permission_groups = permission_group_rows(&permission_groups);
    let selected_module = normalize_rbac_filter(query.module.as_deref());
    let selected_type = normalize_permission_type_filter(query.resource_type.as_deref());
    let query_text = query.q.as_deref().unwrap_or_default().trim();
    let summary_items = permission_summary_items(&permission_groups);
    let module_filters = role_module_filter_rows(&permission_groups, selected_module);
    let type_filters = permission_type_filter_rows(selected_type);
    let filtered_groups = permission_groups
        .iter()
        .filter_map(|group| {
            let permissions = group
                .permissions
                .iter()
                .copied()
                .filter(|permission| {
                    permission_matches_filter(
                        permission,
                        group.module,
                        selected_module,
                        selected_type,
                        query_text,
                    )
                })
                .collect::<Vec<_>>();
            (!permissions.is_empty()).then_some(PermissionGroup {
                id: group.id,
                module: group.module,
                permissions,
            })
        })
        .collect::<Vec<_>>();
    let filtered_count = filtered_groups
        .iter()
        .map(|group| group.permissions.len())
        .sum::<usize>();
    let nav_sections = nav_sections("/admin/permissions", &session);
    render_html(PermissionsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        permission_groups: &filtered_groups,
        module_filters: &module_filters,
        type_filters: &type_filters,
        query: query_text,
        filtered_count,
    })
}

async fn create_account_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_create_account_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.accounts.manage") {
        return forbidden();
    }
    match state
        .auth()
        .create_account(
            &session,
            &form.username,
            &form.display_name,
            &form.password,
            &form.role_ids,
        )
        .await
    {
        Ok(()) => redirect("/admin/accounts?notice=created"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn account_status_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<AccountStatusForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.accounts.manage") {
        return forbidden();
    }
    match state
        .auth()
        .set_account_status(&session, form.account_id, &form.status)
        .await
    {
        Ok(()) => redirect("/admin/accounts?notice=status"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn account_password_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<AccountPasswordForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.accounts.manage") {
        return forbidden();
    }
    match state
        .auth()
        .reset_account_password(&session, form.account_id, &form.password)
        .await
    {
        Ok(()) => redirect("/admin/accounts?notice=password"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn account_roles_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_account_roles_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.accounts.manage") {
        return forbidden();
    }
    match state
        .auth()
        .update_account_roles(&session, form.account_id, &form.role_ids)
        .await
    {
        Ok(()) => redirect("/admin/accounts?notice=roles"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn create_role_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_create_role_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.roles.manage") {
        return forbidden();
    }
    match state
        .auth()
        .create_role(
            &session,
            &form.role_code,
            &form.role_name,
            &form.description,
            &form.permission_ids,
        )
        .await
    {
        Ok(()) => redirect("/admin/roles"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn role_status_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<RoleStatusForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.roles.manage") {
        return forbidden();
    }
    match state
        .auth()
        .set_role_status(&session, form.role_id, &form.status)
        .await
    {
        Ok(()) => redirect("/admin/roles"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn role_permissions_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    RawForm(raw_form): RawForm,
) -> Response {
    let form = match parse_role_permissions_form(raw_form.as_ref()) {
        Ok(form) => form,
        Err(message) => return bad_request(message),
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.roles.manage") {
        return forbidden();
    }
    match state
        .auth()
        .update_role_permissions(&session, form.role_id, &form.permission_ids)
        .await
    {
        Ok(()) => redirect("/admin/roles"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn profile_page(session: CurrentSession) -> Response {
    if !session.can(PROFILE_VIEW) {
        return forbidden();
    }
    let nav_sections = nav_sections("/profile", &session);
    render_html(ProfileTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        username: &session.account.username,
        display_name: session.display_name(),
        role_codes: &session.role_codes,
    })
}

async fn profile_password_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ProfilePasswordForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("profile.password.change") {
        return forbidden();
    }
    match state
        .auth()
        .change_own_password(&session, &form.current_password, &form.new_password)
        .await
    {
        Ok(()) => redirect("/profile"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn sessions_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<SessionsQuery>,
) -> Response {
    if !session.can(RBAC_SESSIONS_VIEW) {
        return forbidden();
    }
    let sessions = match state.auth().list_sessions().await {
        Ok(sessions) => sessions,
        Err(err) => return err.into_response(),
    };
    let selected_status = normalize_rbac_filter(query.status.as_deref());
    let query_text = query.q.as_deref().unwrap_or_default().trim();
    let notice = session_notice_message(query.notice.as_deref());
    let summary_items = session_summary_items(&sessions);
    let status_filters = session_status_filter_rows(selected_status);
    let rows = sessions
        .iter()
        .filter(|item| session_matches_filter(item, selected_status, query_text))
        .map(|item| {
            let (risk_label, risk_tone) = session_risk_view(item);
            SessionRow {
                id: item.id,
                account: if item.display_name.is_empty() {
                    &item.username
                } else {
                    &item.display_name
                },
                status: if item.session_status == "active" {
                    "活跃"
                } else {
                    "已失效"
                },
                status_tone: if item.session_status == "active" {
                    "success"
                } else {
                    "warning"
                },
                last_ip: &item.last_ip,
                user_agent: &item.user_agent,
                risk_label,
                risk_tone,
                created_at: &item.created_at,
                expires_at: &item.refresh_expires_at,
                can_revoke: session.can("rbac.sessions.manage")
                    && item.session_status == "active"
                    && item.id != session.session_id,
            }
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/admin/sessions", &session);
    let can_manage = session.can("rbac.sessions.manage");
    render_html(SessionsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        sessions: &rows,
        status_filters: &status_filters,
        query: query_text,
        filtered_count: rows.len(),
        can_manage,
        notice,
    })
}

async fn session_revoke_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<SessionRevokeForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can("rbac.sessions.manage") {
        return forbidden();
    }
    match state
        .auth()
        .revoke_admin_session(&session, form.session_id)
        .await
    {
        Ok(()) => redirect("/admin/sessions?notice=revoked"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn api_tokens_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ApiTokensQuery>,
) -> Response {
    render_api_tokens_page(&state, &session, None, query.notice.as_deref()).await
}

async fn api_token_create_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<CreateApiTokenForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(API_TOKENS_MANAGE) {
        return forbidden();
    }
    match state.auth().create_api_token(&session, &form.source).await {
        Ok(created) => render_api_tokens_page(&state, &session, Some(created), None).await,
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn api_token_revoke_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ApiTokenRevokeForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(API_TOKENS_MANAGE) {
        return forbidden();
    }
    match state.auth().revoke_api_token(&session, form.token_id).await {
        Ok(()) => redirect("/admin/api-tokens?notice=revoked"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn api_v1_nodes(State(state): State<AppState>, api: ApiSession) -> Response {
    if !api.can(NODES_VIEW) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let nodes = match state.nodes().list_nodes().await {
        Ok(nodes) => nodes,
        Err(err) => return api_error(node_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": nodes.into_iter().map(|node| serde_json::json!({
            "id": node.id,
            "node_key": node.node_key,
            "name": node.name,
            "node_type": node.node_type,
            "address": node.address,
            "ssh_port": node.ssh_port,
            "ssh_user": node.ssh_user,
            "work_dir": node.work_dir,
            "region": node.region,
            "labels": node.labels,
            "status": node.status,
            "docker_status": node.docker_status,
            "capabilities": {
                "docker": node.docker_available == 1,
                "compose": node.compose_available == 1,
                "systemd": node.systemd_available == 1,
                "caddy": node.caddy_available == 1,
                "nginx": node.nginx_available == 1
            },
            "last_check_at": node.last_check_at,
            "last_message": node.last_message
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

async fn api_v1_apps(State(state): State<AppState>, api: ApiSession) -> Response {
    if !api.can(APPS_VIEW) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let apps = match state.apps().list_apps().await {
        Ok(apps) => apps,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": apps.into_iter().map(|app| serde_json::json!({
            "id": app.id,
            "app_key": app.app_key,
            "name": app.name,
            "description": app.description,
            "app_type": app.app_type,
            "deploy_mode": app.deploy_mode,
            "deploy_strategy": app.deploy_strategy,
            "work_dir": app.work_dir,
            "status": app.status,
            "target_names": app.target_names,
            "target_count": app.target_count,
            "created_at": app.created_at,
            "updated_at": app.updated_at
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

async fn api_v1_create_app(
    State(state): State<AppState>,
    api: ApiSession,
    Json(payload): Json<ApiV1CreateAppRequest>,
) -> Response {
    if !api.can("apps.create") {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    if let Err(err) = normalize_deploy_strategy(&payload.deploy_strategy) {
        return api_error(StatusCode::BAD_REQUEST, err.message());
    }
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return api_error(platform_error_status(&err), err.message()),
    };
    let work_dir = if payload.work_dir.trim().is_empty() {
        platform_config.default_app_work_dir_for(&payload.app_key)
    } else {
        payload.work_dir
    };
    let input = CreateAppInput {
        app_key: payload.app_key,
        name: payload.name,
        description: payload.description,
        app_type: payload.app_type,
        deploy_strategy: payload.deploy_strategy,
        work_dir,
        compose_content: payload.compose_content,
        env_content: payload.env_content,
        binary_artifact_version: payload.binary_artifact_version,
        binary_artifact_path: payload.binary_artifact_path,
        binary_exec_args: payload.binary_exec_args,
        binary_service_user: payload.binary_service_user,
        binary_unit_name: payload.binary_unit_name,
        binary_release_strategy: payload.binary_release_strategy,
        binary_active_slot: payload.binary_active_slot,
        binary_base_port: payload.binary_base_port,
        binary_standby_port: payload.binary_standby_port,
        binary_proxy_enabled: payload.binary_proxy_enabled,
        binary_proxy_kind: payload.binary_proxy_kind,
        binary_proxy_domain: payload.binary_proxy_domain,
        binary_proxy_config_path: payload.binary_proxy_config_path,
        target_node_ids: payload.target_node_ids,
    };
    match state.apps().create_app(input).await {
        Ok(app_id) => Json(serde_json::json!({ "data": { "id": app_id } })).into_response(),
        Err(err) => api_error(app_error_status(&err), err.message()),
    }
}

async fn api_v1_app_detail(
    State(state): State<AppState>,
    api: ApiSession,
    Path(app_id): Path<i64>,
) -> Response {
    if !api.can(APPS_VIEW) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": {
            "app": {
                "id": detail.app.id,
                "app_key": detail.app.app_key,
                "name": detail.app.name,
                "description": detail.app.description,
                "app_type": detail.app.app_type,
                "deploy_mode": detail.app.deploy_mode,
                "deploy_strategy": detail.app.deploy_strategy,
                "work_dir": detail.app.work_dir,
                "status": detail.app.status,
                "target_names": detail.app.target_names,
                "target_count": detail.app.target_count,
                "created_at": detail.app.created_at,
                "updated_at": detail.app.updated_at
            },
            "runtime_root": detail.runtime_root,
            "compose_content": detail.compose_content,
            "env_content": detail.env_content,
            "service_names": detail.service_names,
            "target_nodes": detail.target_nodes.into_iter().map(|node| serde_json::json!({
                "id": node.id,
                "name": node.name,
                "node_key": node.node_key,
                "node_type": node.node_type,
                "status": node.status,
                "docker_status": node.docker_status
            })).collect::<Vec<_>>(),
            "runtime_states": detail.runtime_states.into_iter().map(|state| serde_json::json!({
                "node_id": state.node_id,
                "node_name": state.node_name,
                "node_key": state.node_key,
                "runtime_status": state.runtime_status,
                "active_version": state.active_version,
                "service_count": state.service_count,
                "message": state.message,
                "last_task_id": state.last_task_id,
                "last_task_status": state.last_task_status,
                "last_task_kind": state.last_task_kind,
                "last_deploy_at": state.last_deploy_at,
                "updated_at": state.updated_at
            })).collect::<Vec<_>>(),
            "binary_config": {
                "artifact_version": detail.binary_config.artifact_version,
                "artifact_path": detail.binary_config.artifact_path,
                "exec_args": detail.binary_config.exec_args,
                "service_user": detail.binary_config.service_user,
                "unit_name": detail.binary_config.unit_name,
                "release_strategy": detail.binary_config.release_strategy,
                "active_slot": detail.binary_config.active_slot,
                "base_port": detail.binary_config.base_port,
                "standby_port": detail.binary_config.standby_port,
                "proxy_enabled": detail.binary_config.proxy_enabled == 1,
                "proxy_kind": detail.binary_config.proxy_kind,
                "proxy_domain": detail.binary_config.proxy_domain,
                "proxy_config_path": detail.binary_config.proxy_config_path
            }
        }
    }))
    .into_response()
}

async fn api_v1_deploy_app(
    State(state): State<AppState>,
    api: ApiSession,
    Path(app_id): Path<i64>,
    Json(payload): Json<ApiV1DeployAppRequest>,
) -> Response {
    if !api.can("services.deploy") {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let actor = api.actor();
    let result = match payload.action.as_str() {
        "up" | "compose_up" => {
            state
                .apps()
                .run_compose_task(app_id, ComposeTaskAction::Up, &actor)
                .await
        }
        "down" | "compose_down" => {
            state
                .apps()
                .run_compose_task(app_id, ComposeTaskAction::Down, &actor)
                .await
        }
        "restart" | "compose_restart" => {
            state
                .apps()
                .run_compose_task(app_id, ComposeTaskAction::Restart, &actor)
                .await
        }
        "binary_restart" => {
            state
                .apps()
                .run_binary_task(app_id, BinaryTaskAction::Restart, &actor)
                .await
        }
        "binary_stop" => {
            state
                .apps()
                .run_binary_task(app_id, BinaryTaskAction::Stop, &actor)
                .await
        }
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "unsupported deploy action, use up/down/restart/binary_restart/binary_stop",
            );
        }
    };
    match result {
        Ok(task_id) => Json(serde_json::json!({ "data": { "task_id": task_id } })).into_response(),
        Err(err) => api_error(app_error_status(&err), err.message()),
    }
}

async fn api_v1_tasks(
    State(state): State<AppState>,
    api: ApiSession,
    Query(query): Query<ApiV1TaskListQuery>,
) -> Response {
    if !api.can(TASKS_VIEW) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let tasks = match state
        .tasks()
        .list_tasks_filtered(TaskListFilter {
            status: query.status,
            phase: query.phase,
            app_id: query.app_id,
            task_kind: query.task_kind,
            query: query.q,
        })
        .await
    {
        Ok(tasks) => tasks,
        Err(err) => return api_error(task_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": tasks.into_iter().map(|task| serde_json::json!({
            "id": task.id,
            "task_kind": task.task_kind,
            "title": task.title,
            "app_name": task.app_name,
            "status": task.status,
            "phase": task.phase,
            "command": task.command,
            "summary": task.summary,
            "exit_code": task.exit_code,
            "created_by": task.created_by,
            "created_at": task.created_at,
            "updated_at": task.updated_at
        })).collect::<Vec<_>>()
    }))
    .into_response()
}

async fn api_v1_task_detail(
    State(state): State<AppState>,
    api: ApiSession,
    Path(task_id): Path<i64>,
) -> Response {
    if !api.can(TASKS_VIEW) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let task = match state.tasks().task_detail(task_id).await {
        Ok(task) => task,
        Err(err) => return api_error(task_error_status(&err), err.message()),
    };
    let logs = match state.tasks().task_logs(task_id).await {
        Ok(logs) => logs,
        Err(err) => return api_error(task_error_status(&err), err.message()),
    };
    let node_results = match state.tasks().task_node_results(task_id).await {
        Ok(results) => results,
        Err(err) => return api_error(task_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": {
            "task": {
                "id": task.id,
                "app_id": task.app_id,
                "node_id": task.node_id,
                "task_kind": task.task_kind,
                "title": task.title,
                "app_name": task.app_name,
                "node_name": task.node_name,
                "status": task.status,
                "phase": task.phase,
                "command": task.command,
                "summary": task.summary,
                "exit_code": task.exit_code,
                "created_by": task.created_by,
                "started_at": task.started_at,
                "finished_at": task.finished_at,
                "created_at": task.created_at,
                "updated_at": task.updated_at
            },
            "logs": logs.into_iter().map(|log| serde_json::json!({
                "id": log.id,
                "stream": log.stream,
                "content": log.content,
                "created_at": log.created_at
            })).collect::<Vec<_>>(),
            "node_results": node_results.into_iter().map(|result| serde_json::json!({
                "id": result.id,
                "node_id": result.node_id,
                "node_name": result.node_name,
                "node_key": result.node_key,
                "node_type": result.node_type,
                "status": result.status,
                "message": result.message,
                "command_count": result.command_count,
                "started_at": result.started_at,
                "finished_at": result.finished_at
            })).collect::<Vec<_>>()
        }
    }))
    .into_response()
}

async fn openapi_json() -> impl IntoResponse {
    Json(openapi_spec())
}

async fn openapi_docs() -> impl IntoResponse {
    Html(openapi_docs_html())
}

async fn render_api_tokens_page(
    state: &AppState,
    session: &CurrentSession,
    created: Option<crate::auth::CreatedApiToken>,
    notice: Option<&str>,
) -> Response {
    if !session.can(API_TOKENS_VIEW) {
        return forbidden();
    }
    let tokens = match state.auth().list_api_tokens().await {
        Ok(tokens) => tokens,
        Err(err) => return err.into_response(),
    };
    let rows = tokens
        .iter()
        .map(|token| ApiTokenPageRow {
            id: token.id,
            account: if token.display_name.is_empty() {
                token.username.clone()
            } else {
                format!("{} ({})", token.display_name, token.username)
            },
            token_prefix: &token.token_prefix,
            source: &token.source,
            status: api_token_status_label(&token.status),
            status_tone: api_token_status_tone(&token.status),
            last_used_at: token
                .last_used_at
                .clone()
                .unwrap_or_else(|| "未使用".to_owned()),
            last_used_ip: &token.last_used_ip,
            created_at: &token.created_at,
            revoked_at: token.revoked_at.as_deref().unwrap_or("已吊销"),
            can_revoke: session.can(API_TOKENS_MANAGE) && token.status == "active",
        })
        .collect::<Vec<_>>();
    let summary_items = api_token_summary_items(&tokens);
    let nav_sections = nav_sections("/admin/api-tokens", session);
    let created_token = created.as_ref().map(|token| token.token.as_str());
    let created_source = created.as_ref().map(|token| token.source.as_str());
    let created_prefix = created.as_ref().map(|token| token.token_prefix.as_str());
    render_html(ApiTokensTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        summary_items: &summary_items,
        tokens: &rows,
        created_token,
        created_source,
        created_prefix,
        can_manage: session.can(API_TOKENS_MANAGE),
        notice: api_token_notice_message(notice),
    })
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(state.db())
        .await
    {
        Ok(1) => (StatusCode::OK, "ok").into_response(),
        Ok(_) => (StatusCode::SERVICE_UNAVAILABLE, "database check failed").into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("database check failed: {err}"),
        )
            .into_response(),
    }
}

async fn logo_svg() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        LOGO_SVG,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        APP_JS,
    )
}

fn node_status_label(status: &str) -> &'static str {
    match status {
        "online" => "在线",
        "offline" => "离线",
        "disabled" => "已禁用",
        _ => "未探测",
    }
}

fn node_status_tone(status: &str) -> &'static str {
    match status {
        "online" => "success",
        "offline" | "disabled" => "warning",
        _ => "neutral",
    }
}

fn node_page_row<'a>(node: &'a crate::nodes::NodeListItem, can_manage: bool) -> NodePageRow<'a> {
    NodePageRow {
        id: node.id,
        name: &node.name,
        node_key: &node.node_key,
        node_type: node_type_label(&node.node_type),
        address: &node.address,
        ssh: if node.node_type == "ssh" {
            format!("{}:{}", node.ssh_user, node.ssh_port)
        } else {
            "本地执行".to_owned()
        },
        ssh_port: node.ssh_port,
        ssh_user: &node.ssh_user,
        credential_id: node.credential_id.unwrap_or_default(),
        credential_name: node_credential_display_name(node),
        credential_fingerprint: node_credential_fingerprint(node),
        work_dir: &node.work_dir,
        region: if node.region.is_empty() {
            "未分区"
        } else {
            &node.region
        },
        region_value: &node.region,
        labels: if node.labels.is_empty() {
            "未设置"
        } else {
            &node.labels
        },
        labels_value: &node.labels,
        status: node_status_label(&node.status),
        status_tone: node_status_tone(&node.status),
        docker_status: &node.docker_status,
        capability: node_capability_text(node),
        os_info: node_probe_detail_text(node.last_os_info.as_deref(), "OS 未探测"),
        disk_info: node_disk_detail_text(node.last_disk_info.as_deref(), "磁盘未探测"),
        systemd_version: node_probe_detail_text(
            node.last_systemd_version.as_deref(),
            "systemd 未探测",
        ),
        proxy_version: node_proxy_version_text(node),
        last_check_at: node.last_check_at.as_deref().unwrap_or("尚未探测"),
        last_message: node.last_message.as_deref().unwrap_or("等待节点探测"),
        can_manage,
        is_ssh: node.node_type == "ssh",
        can_check: node.status != "disabled",
        toggle_status: node_status_toggle_value(&node.status),
        toggle_label: node_status_toggle_label(&node.status),
    }
}

fn node_check_history_row(check: &crate::nodes::NodeCheckHistoryItem) -> NodeCheckHistoryRow {
    NodeCheckHistoryRow {
        id: check.id,
        status: node_check_status_label(&check.check_status),
        status_tone: node_check_status_tone(&check.check_status),
        message: display_text(check.message.clone(), "未记录"),
        docker_version: display_text(check.docker_version.clone(), "未记录"),
        compose_version: display_text(check.compose_version.clone(), "未记录"),
        os_info: node_probe_detail_text(Some(&check.os_info), "OS 未探测"),
        disk_info: node_disk_detail_text(Some(&check.disk_info), "磁盘未探测"),
        systemd_version: node_probe_detail_text(Some(&check.systemd_version), "systemd 未探测"),
        checked_at: check.checked_at.clone(),
    }
}

fn node_app_runtime_row(app: &crate::nodes::NodeAppRuntimeItem) -> NodeAppRuntimeRow {
    NodeAppRuntimeRow {
        app_id: app.app_id,
        app_name: app.app_name.clone(),
        app_key: app.app_key.clone(),
        app_type: app_type_label(&app.app_type),
        app_status: app_status_label(&app.app_status),
        app_status_tone: app_status_tone(&app.app_status),
        runtime_status: runtime_status_label(&app.runtime_status),
        runtime_status_tone: runtime_status_tone(&app.runtime_status),
        active_version: display_text(app.active_version.clone(), "未部署"),
        service_count: app.service_count,
        message: display_text(app.message.clone(), "暂无运行信息"),
        last_deploy_at: app
            .last_deploy_at
            .clone()
            .unwrap_or_else(|| "未部署".to_owned()),
        updated_at: app.updated_at.clone(),
    }
}

fn node_task_row(task: &crate::nodes::NodeTaskItem) -> NodeTaskRow {
    NodeTaskRow {
        id: task.id,
        title: task.title.clone(),
        task_kind: task_kind_label(&task.task_kind),
        app_name: display_text(task.app_name.clone(), "未关联应用"),
        status: task_status_label(&task.status),
        status_tone: task_status_tone(&task.status),
        phase: task_phase_label(&task.phase),
        summary: display_text(task.summary.clone(), "暂无摘要"),
        created_by: task.created_by.clone(),
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
    }
}

fn node_check_result_node_status_label(status: &str) -> &'static str {
    if status == "passed" {
        node_status_label("online")
    } else {
        node_status_label("offline")
    }
}

fn node_check_result_node_status_tone(status: &str) -> &'static str {
    if status == "passed" {
        node_status_tone("online")
    } else {
        node_status_tone("offline")
    }
}

fn node_check_result_docker_status(result: &crate::nodes::NodeCheckResult) -> &'static str {
    if result.status == "passed" {
        "available"
    } else if result.docker_version.is_empty() {
        "unknown"
    } else {
        "failed"
    }
}

fn node_type_label(node_type: &str) -> &'static str {
    match node_type {
        "local" => "本机",
        "ssh" => "SSH",
        _ => "未知类型",
    }
}

fn deploy_confirm_docker_status(status: &str) -> String {
    match status {
        "available" => "Docker 可用".to_owned(),
        "unavailable" => "Docker 不可用".to_owned(),
        "unknown" | "" => "Docker 未探测".to_owned(),
        other => other.to_owned(),
    }
}

fn deploy_confirm_target_hint(status: &str, docker_status: &str) -> &'static str {
    match status {
        "offline" => "提交后会在预检阶段阻断",
        "unknown" => "建议先完成节点探测",
        "online" if docker_status == "available" => "预检将继续校验部署环境",
        "online" => "预检会重新校验 Docker 与 Compose",
        _ => "提交前请确认节点状态",
    }
}

fn deploy_plan_failure_policy(strategy: &str) -> &'static str {
    match strategy {
        "rolling_continue" => "某个节点失败后继续执行后续节点，任务最终汇总为失败。",
        _ => "某个节点失败后停止后续节点执行，未执行节点会标记为跳过。",
    }
}

fn deploy_plan_steps(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> Vec<DeployPlanStepRow> {
    let mut rows = Vec::new();
    rows.push(DeployPlanStepRow {
        label: "1. 进入队列",
        detail: format!(
            "创建 {} 任务，按目标节点顺序滚动执行。",
            deploy_confirm_action_label(action)
        ),
        tone: "active",
    });
    rows.push(DeployPlanStepRow {
        label: "2. 节点预检",
        detail: deploy_plan_preflight_detail(action),
        tone: "neutral",
    });
    if deploy_confirm_syncs_files(action) {
        rows.push(DeployPlanStepRow {
            label: "3. 同步运行文件",
            detail: deploy_plan_sync_detail(detail, action),
            tone: "neutral",
        });
    }
    if matches!(
        action,
        DeployConfirmAction::Binary(BinaryTaskAction::Restart)
    ) && detail.binary_config.release_strategy == "blue_green"
    {
        rows.push(DeployPlanStepRow {
            label: "Blue/Green 预案",
            detail: deploy_plan_blue_green_detail(detail),
            tone: "warning",
        });
        if detail.binary_config.proxy_enabled == 1 {
            rows.push(DeployPlanStepRow {
                label: "反向代理切流",
                detail: deploy_plan_proxy_switch_detail(detail),
                tone: "active",
            });
        }
    }
    rows.push(DeployPlanStepRow {
        label: deploy_plan_execute_label(action),
        detail: deploy_plan_execute_detail(detail, action),
        tone: deploy_confirm_action_tone(action),
    });
    if deploy_confirm_runs_health_check(action) {
        rows.push(DeployPlanStepRow {
            label: "健康检查",
            detail: format!(
                "执行 {}，超时 {} 秒；失败时当前节点会标记为异常。",
                detail.health_check.kind.label(),
                detail.health_check.timeout_secs
            ),
            tone: "success",
        });
    }
    rows.push(DeployPlanStepRow {
        label: "结果回写",
        detail: "记录任务日志、节点结果、部署历史和应用运行状态。".to_owned(),
        tone: "neutral",
    });
    rows
}

fn deploy_plan_files(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> Vec<DeployPlanFileRow> {
    match action {
        DeployConfirmAction::Compose(_) => vec![
            DeployPlanFileRow {
                label: "Compose",
                path: target_work_dir_path(&detail.app.work_dir, "compose.yaml"),
                detail: "服务编排文件",
            },
            DeployPlanFileRow {
                label: "环境变量",
                path: target_work_dir_path(&detail.app.work_dir, ".env"),
                detail: "可为空",
            },
            DeployPlanFileRow {
                label: "应用元数据",
                path: target_work_dir_path(&detail.app.work_dir, ".easy-deploy/app.yaml"),
                detail: "记录应用、节点和策略",
            },
        ],
        DeployConfirmAction::Binary(_) => {
            let unit_name = if detail.binary_config.unit_name.trim().is_empty() {
                format!("easy-deploy-{}.service", detail.app.app_key)
            } else {
                detail.binary_config.unit_name.clone()
            };
            let env_file = binary_unit_env_file_name(&unit_name);
            let release = if detail.binary_config.artifact_version.trim().is_empty() {
                "未配置版本".to_owned()
            } else {
                detail.binary_config.artifact_version.clone()
            };
            let mut files = vec![
                DeployPlanFileRow {
                    label: "systemd unit",
                    path: target_work_dir_path(
                        &detail.app.work_dir,
                        &format!(".easy-deploy/systemd/{unit_name}"),
                    ),
                    detail: "服务定义",
                },
                DeployPlanFileRow {
                    label: "环境变量",
                    path: target_work_dir_path(
                        &detail.app.work_dir,
                        &format!(".easy-deploy/systemd/{env_file}"),
                    ),
                    detail: "EnvironmentFile",
                },
                DeployPlanFileRow {
                    label: "发布版本",
                    path: target_work_dir_path(
                        &detail.app.work_dir,
                        &format!("releases/{release}/release.yaml"),
                    ),
                    detail: "release 元数据",
                },
                DeployPlanFileRow {
                    label: "当前指针",
                    path: target_work_dir_path(&detail.app.work_dir, "current"),
                    detail: "当前 release",
                },
                DeployPlanFileRow {
                    label: "切流预案",
                    path: target_work_dir_path(&detail.app.work_dir, ".easy-deploy/app.yaml"),
                    detail: "记录 Blue/Green 槽位和端口",
                },
            ];
            if detail.binary_config.proxy_enabled == 1 {
                files.push(DeployPlanFileRow {
                    label: "反向代理",
                    path: detail.binary_config.proxy_config_path.clone(),
                    detail: "Caddy/Nginx 切流配置",
                });
            }
            files
        }
    }
}

fn deploy_preflight_rows(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> Vec<DeployPreflightRow> {
    detail
        .target_nodes
        .iter()
        .map(|node| {
            let checks = deploy_preflight_checks(detail, node, action);
            let actions = deploy_preflight_actions(detail, node, action);
            let block_count = checks
                .iter()
                .filter(|check| check.tone == "warning")
                .count();
            let warning_count = checks
                .iter()
                .filter(|check| check.tone == "neutral")
                .count();
            let (status, status_tone, summary) = if block_count > 0 {
                (
                    "预计阻断",
                    "warning",
                    format!("{block_count} 项会在任务预检阶段阻断"),
                )
            } else if warning_count > 0 {
                (
                    "建议确认",
                    "neutral",
                    format!("{warning_count} 项需要提交前确认"),
                )
            } else {
                ("可提交", "success", "已知能力满足本次操作".to_owned())
            };

            DeployPreflightRow {
                node_id: node.id,
                node_name: node.name.clone(),
                node_key: node.node_key.clone(),
                status,
                status_tone,
                summary,
                checks,
                actions,
            }
        })
        .collect()
}

async fn deploy_submit_blocker(
    state: &AppState,
    app_id: i64,
    action: DeployConfirmAction,
) -> Result<Option<String>, AppError> {
    let detail = state.apps().app_detail(app_id).await?;
    let rows = deploy_preflight_rows(&detail, action);
    Ok(deploy_preflight_submit_blocker(&rows))
}

fn deploy_preflight_submit_state(rows: &[DeployPreflightRow]) -> (bool, String) {
    match deploy_preflight_submit_blocker(rows) {
        Some(message) => (false, message),
        None => (
            true,
            "当前没有已知阻断项；提交后仍会执行任务级预检。".to_owned(),
        ),
    }
}

fn deploy_preflight_submit_blocker(rows: &[DeployPreflightRow]) -> Option<String> {
    let blocked_nodes = rows
        .iter()
        .filter(|row| row.status_tone == "warning")
        .map(|row| row.node_name.as_str())
        .collect::<Vec<_>>();
    if blocked_nodes.is_empty() {
        None
    } else {
        Some(format!(
            "{} 个目标节点存在已知阻断项：{}。请先完成节点探测、安装缺失组件或调整目标节点后再提交。",
            blocked_nodes.len(),
            blocked_nodes.join("、")
        ))
    }
}

fn deploy_preflight_checks(
    detail: &crate::apps::AppConfigDetail,
    node: &crate::apps::AppTargetSummaryItem,
    action: DeployConfirmAction,
) -> Vec<DeployPreflightCheckRow> {
    let mut checks = Vec::new();
    checks.push(deploy_preflight_check(
        "节点状态",
        match node.status.as_str() {
            "online" => ("通过", "success", "节点最近一次探测在线".to_owned()),
            "offline" => (
                "阻断",
                "warning",
                "节点当前离线，任务预检会直接失败".to_owned(),
            ),
            "unknown" | "" => (
                "待确认",
                "neutral",
                "节点尚未确认在线，建议先执行探测".to_owned(),
            ),
            other => (
                "待确认",
                "neutral",
                format!("节点状态为 {other}，提交前请确认"),
            ),
        },
    ));

    match action {
        DeployConfirmAction::Compose(_) => {
            checks.push(capability_preflight_check(
                "Docker",
                node.docker_available == 1,
                &node.capability_message,
                "Docker CLI 与 daemon 已通过探测",
                "Docker 未通过探测，Compose 部署大概率会失败",
            ));
            checks.push(capability_preflight_check(
                "Compose",
                node.compose_available == 1,
                &node.capability_message,
                "Docker Compose 插件已通过探测",
                "Docker Compose 未通过探测，无法执行 compose 命令",
            ));
        }
        DeployConfirmAction::Binary(_) => {
            checks.push(capability_preflight_check(
                "systemd",
                node.systemd_available == 1,
                &node.capability_message,
                "systemd 已通过探测",
                "systemd 未通过探测，二进制服务无法 restart/stop",
            ));
            if matches!(
                action,
                DeployConfirmAction::Binary(BinaryTaskAction::Restart)
            ) && detail.binary_config.proxy_enabled == 1
            {
                match detail.binary_config.proxy_kind.as_str() {
                    "caddy" => checks.push(capability_preflight_check(
                        "Caddy",
                        node.caddy_available == 1,
                        &node.capability_message,
                        "Caddy 已通过探测，支持反向代理切流",
                        "已启用 Caddy 切流，但目标节点未通过 Caddy 探测",
                    )),
                    "nginx" => checks.push(capability_preflight_check(
                        "Nginx",
                        node.nginx_available == 1,
                        &node.capability_message,
                        "Nginx 已通过探测，支持反向代理切流",
                        "已启用 Nginx 切流，但目标节点未通过 Nginx 探测",
                    )),
                    _ => {}
                }
            }
        }
    }

    checks
}

fn deploy_preflight_actions(
    detail: &crate::apps::AppConfigDetail,
    node: &crate::apps::AppTargetSummaryItem,
    action: DeployConfirmAction,
) -> Vec<DeployPreflightActionRow> {
    let mut actions = Vec::new();
    if node.status != "online" {
        actions.push(DeployPreflightActionRow {
            label: "立即探测",
            action_kind: "check",
            component: "",
        });
    }
    match action {
        DeployConfirmAction::Compose(_) => {
            if node.docker_available == 0 {
                actions.push(deploy_install_action("安装 Docker", "docker"));
            }
            if node.compose_available == 0 {
                actions.push(deploy_install_action("安装 Compose", "compose"));
            }
        }
        DeployConfirmAction::Binary(BinaryTaskAction::Restart) => {
            if node.systemd_available == 0 {
                actions.push(DeployPreflightActionRow {
                    label: "查看 systemd 建议",
                    action_kind: "detail",
                    component: "",
                });
            }
            if detail.binary_config.proxy_enabled == 1 {
                match detail.binary_config.proxy_kind.as_str() {
                    "caddy" if node.caddy_available == 0 => {
                        actions.push(deploy_install_action("安装 Caddy", "caddy"));
                    }
                    "nginx" if node.nginx_available == 0 => {
                        actions.push(deploy_install_action("安装 Nginx", "nginx"));
                    }
                    _ => {}
                }
            }
        }
        DeployConfirmAction::Binary(BinaryTaskAction::Stop) => {
            if node.systemd_available == 0 {
                actions.push(DeployPreflightActionRow {
                    label: "查看 systemd 建议",
                    action_kind: "detail",
                    component: "",
                });
            }
        }
    }
    if actions.is_empty() && node.status == "offline" {
        actions.push(DeployPreflightActionRow {
            label: "查看节点详情",
            action_kind: "detail",
            component: "",
        });
    }
    actions
}

fn deploy_install_action(label: &'static str, component: &'static str) -> DeployPreflightActionRow {
    DeployPreflightActionRow {
        label,
        action_kind: "install",
        component,
    }
}

fn deploy_preflight_check(
    label: &'static str,
    (result, tone, detail): (&'static str, &'static str, String),
) -> DeployPreflightCheckRow {
    DeployPreflightCheckRow {
        label,
        result,
        tone,
        detail,
    }
}

fn capability_preflight_check(
    label: &'static str,
    available: bool,
    message: &str,
    pass_detail: &'static str,
    fail_detail: &'static str,
) -> DeployPreflightCheckRow {
    if available {
        return deploy_preflight_check(label, ("通过", "success", pass_detail.to_owned()));
    }
    let message = message.trim();
    let detail = if message.is_empty() {
        fail_detail.to_owned()
    } else {
        format!("{fail_detail}：{}", first_line(message))
    };
    deploy_preflight_check(label, ("阻断", "warning", detail))
}

fn deploy_preflight_summary(rows: &[DeployPreflightRow]) -> (String, &'static str) {
    let blocked = rows
        .iter()
        .filter(|row| row.status_tone == "warning")
        .count();
    let warnings = rows
        .iter()
        .filter(|row| row.status_tone == "neutral")
        .count();
    let ready = rows.len().saturating_sub(blocked + warnings);
    let tone = if blocked > 0 {
        "warning"
    } else if warnings > 0 {
        "neutral"
    } else {
        "success"
    };
    (
        format!("{ready} 个可提交，{warnings} 个待确认，{blocked} 个预计阻断"),
        tone,
    )
}

fn deploy_plan_preflight_detail(action: DeployConfirmAction) -> String {
    match action {
        DeployConfirmAction::Compose(_) => {
            "校验节点在线状态、Docker daemon、docker compose config、部署目录和磁盘空间。"
                .to_owned()
        }
        DeployConfirmAction::Binary(_) => {
            "校验节点在线状态、systemd 可用性、部署目录和发布文件。".to_owned()
        }
    }
}

fn deploy_plan_sync_detail(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> String {
    match action {
        DeployConfirmAction::Compose(_) => {
            format!(
                "把 compose.yaml、.env 和 app.yaml 同步到 {}。",
                detail.app.work_dir
            )
        }
        DeployConfirmAction::Binary(_) => {
            format!(
                "把 releases、current、systemd unit/env 和 app.yaml 同步到 {}。",
                detail.app.work_dir
            )
        }
    }
}

fn deploy_plan_blue_green_detail(detail: &crate::apps::AppConfigDetail) -> String {
    let proxy = if detail.binary_config.proxy_enabled == 1 {
        format!(
            "健康检查通过后会切换 {} 到备用槽位。",
            binary_proxy_kind_label(&detail.binary_config.proxy_kind)
        )
    } else {
        "未启用反向代理切流。".to_owned()
    };
    format!(
        "当前槽位 {}，备用槽位 {}；主槽端口 {}，备用槽端口 {}。本次会启动并检查备用槽位 systemd unit，{}",
        detail.binary_config.active_slot,
        binary_standby_slot(&detail.binary_config.active_slot),
        port_plan_text(detail.binary_config.base_port),
        port_plan_text(detail.binary_config.standby_port),
        proxy,
    )
}

fn deploy_plan_proxy_switch_detail(detail: &crate::apps::AppConfigDetail) -> String {
    format!(
        "生成 {} 配置 {} -> 127.0.0.1:{}，validate 成功后 reload；失败则不记录新槽位。",
        binary_proxy_kind_label(&detail.binary_config.proxy_kind),
        display_text(detail.binary_config.proxy_domain.clone(), "未配置域名"),
        port_plan_text(
            match binary_standby_slot(&detail.binary_config.active_slot) {
                "green" => detail.binary_config.standby_port,
                _ => detail.binary_config.base_port,
            }
        )
    )
}

fn deploy_plan_execute_label(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => "执行部署",
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => "执行停止",
        DeployConfirmAction::Compose(ComposeTaskAction::Restart)
        | DeployConfirmAction::Binary(BinaryTaskAction::Restart) => "执行重启",
        DeployConfirmAction::Binary(BinaryTaskAction::Stop) => "执行停止",
    }
}

fn deploy_plan_execute_detail(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> String {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => {
            "运行 docker compose up -d --remove-orphans。".to_owned()
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => {
            "运行 docker compose down。".to_owned()
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => {
            "运行 docker compose restart。".to_owned()
        }
        DeployConfirmAction::Binary(BinaryTaskAction::Restart) => {
            let unit_name = if detail.binary_config.release_strategy == "blue_green" {
                binary_blue_green_unit_name(
                    &detail.binary_config.unit_name,
                    binary_standby_slot(&detail.binary_config.active_slot),
                )
            } else {
                detail.binary_config.unit_name.clone()
            };
            format!(
                "{}执行 systemctl link、daemon-reload，然后 restart {}。",
                binary_restart_plan_prefix(&detail.binary_config.release_strategy),
                display_text(unit_name, "未配置 unit")
            )
        }
        DeployConfirmAction::Binary(BinaryTaskAction::Stop) => format!(
            "执行 systemctl stop {}。",
            display_text(detail.binary_config.unit_name.clone(), "未配置 unit")
        ),
    }
}

fn binary_restart_plan_prefix(strategy: &str) -> &'static str {
    if strategy == "blue_green" {
        "Blue/Green 会使用备用槽位 unit；"
    } else {
        ""
    }
}

fn binary_blue_green_unit_name(unit_name: &str, slot: &str) -> String {
    let stem = unit_name.strip_suffix(".service").unwrap_or(unit_name);
    format!("{stem}-{slot}.service")
}

fn port_plan_text(port: i64) -> String {
    if port > 0 {
        port.to_string()
    } else {
        "未配置".to_owned()
    }
}

fn deploy_confirm_syncs_files(action: DeployConfirmAction) -> bool {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up)
        | DeployConfirmAction::Compose(ComposeTaskAction::Restart)
        | DeployConfirmAction::Compose(ComposeTaskAction::Down) => true,
        DeployConfirmAction::Binary(BinaryTaskAction::Restart) => true,
        DeployConfirmAction::Binary(BinaryTaskAction::Stop) => false,
    }
}

fn deploy_confirm_runs_health_check(action: DeployConfirmAction) -> bool {
    matches!(
        action,
        DeployConfirmAction::Compose(ComposeTaskAction::Up)
            | DeployConfirmAction::Compose(ComposeTaskAction::Restart)
            | DeployConfirmAction::Binary(BinaryTaskAction::Restart)
    )
}

fn node_status_toggle_value(status: &str) -> &'static str {
    if status == "disabled" {
        "unknown"
    } else {
        "disabled"
    }
}

fn node_status_toggle_label(status: &str) -> &'static str {
    if status == "disabled" {
        "启用"
    } else {
        "禁用"
    }
}

fn node_credential_display_name(node: &crate::nodes::NodeListItem) -> String {
    if node.node_type != "ssh" {
        return "本地执行".to_owned();
    }
    node.credential_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("系统 SSH 配置")
        .to_owned()
}

fn node_credential_fingerprint(node: &crate::nodes::NodeListItem) -> String {
    node.credential_fingerprint
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("")
        .to_owned()
}

fn node_capability_text(node: &crate::nodes::NodeListItem) -> String {
    let executor = if node.node_type == "local" {
        "本地执行"
    } else {
        "SSH 执行"
    };
    let docker = node
        .last_docker_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(node.docker_status.as_str());
    let compose = node
        .last_compose_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("Compose 未探测");
    let proxy = node_proxy_capability_text(node);
    format!("{executor} · {docker} · {compose} · {proxy}")
}

fn node_proxy_capability_text(node: &crate::nodes::NodeListItem) -> String {
    match (node.caddy_available == 1, node.nginx_available == 1) {
        (true, true) => "Caddy/Nginx 可用".to_owned(),
        (true, false) => "Caddy 可用".to_owned(),
        (false, true) => "Nginx 可用".to_owned(),
        (false, false) => "代理未探测".to_owned(),
    }
}

fn node_proxy_version_text(node: &crate::nodes::NodeListItem) -> String {
    let mut versions = Vec::new();
    if let Some(caddy) = node
        .last_caddy_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        versions.push(format!("Caddy {}", first_line(caddy)));
    }
    if let Some(nginx) = node
        .last_nginx_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        versions.push(first_line(nginx).to_owned());
    }
    if versions.is_empty() {
        "代理未探测".to_owned()
    } else {
        versions.join(" · ")
    }
}

fn node_probe_detail_text(value: Option<&str>, fallback: &'static str) -> String {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.lines().next().unwrap_or(value).trim().to_owned()
    }
}

fn node_disk_detail_text(value: Option<&str>, fallback: &'static str) -> String {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        return fallback.to_owned();
    }
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("Filesystem"))
        .unwrap_or(value)
        .to_owned()
}

fn node_capability_guides(node: &crate::nodes::NodeListItem) -> Vec<NodeCapabilityGuideRow> {
    let mut guides = Vec::new();
    let install_prefix = if node.node_type == "ssh" {
        let identity_args = node
            .credential_private_key_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|path| format!(" -i {path} -o IdentitiesOnly=yes"))
            .unwrap_or_default();
        format!(
            "ssh -p {}{} {}@{} ",
            node.ssh_port, identity_args, node.ssh_user, node.address
        )
    } else {
        String::new()
    };

    if node.docker_available == 0 {
        guides.push(NodeCapabilityGuideRow {
            title: "安装 Docker Engine",
            tone: "warning",
            reason: capability_reason(
                node.last_message.as_deref(),
                "节点还没有通过 Docker CLI 与 daemon 检查，Compose 应用无法部署。",
            ),
            command: format!(
                "{install_prefix}curl -fsSL https://get.docker.com | sudo sh && sudo systemctl enable --now docker"
            ),
            verify: "重新探测后应看到 Docker 版本和 online 状态。",
            install_component: "docker",
            can_install: true,
        });
    }

    if node.docker_available == 1 && node.compose_available == 0 {
        guides.push(NodeCapabilityGuideRow {
            title: "安装 Docker Compose 插件",
            tone: "warning",
            reason: capability_reason(
                node.last_message.as_deref(),
                "Docker 可用，但 docker compose version 未通过，Compose 应用无法执行。",
            ),
            command: format!(
                "{install_prefix}sudo apt-get update && sudo apt-get install -y docker-compose-plugin"
            ),
            verify: "重新探测后应看到 Docker Compose version v2.x。",
            install_component: "compose",
            can_install: true,
        });
    }

    if node.systemd_available == 0 {
        guides.push(NodeCapabilityGuideRow {
            title: "确认 systemd 可用",
            tone: "neutral",
            reason: capability_reason(
                node.last_systemd_version.as_deref(),
                "二进制直接部署需要 systemd 管理服务；容器、极简系统或权限不足时可能不可用。",
            ),
            command: format!("{install_prefix}systemctl --version && systemctl status"),
            verify: "重新探测后 systemd 应显示版本号，而不是探测失败信息。",
            install_component: "",
            can_install: false,
        });
    }

    if node.caddy_available == 0 {
        guides.push(NodeCapabilityGuideRow {
            title: "安装 Caddy",
            tone: "neutral",
            reason: capability_reason(
                node.last_caddy_version.as_deref(),
                "启用 Caddy 反向代理切流时，目标节点需要 caddy 命令和 caddy.service。",
            ),
            command: format!(
                "{install_prefix}sudo apt-get update && sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt | sudo tee /etc/apt/sources.list.d/caddy-stable.list && sudo apt-get update && sudo apt-get install -y caddy"
            ),
            verify: "重新探测后应看到 Caddy 版本，部署确认页不再阻断 Caddy 切流。",
            install_component: "caddy",
            can_install: true,
        });
    }

    if node.nginx_available == 0 {
        guides.push(NodeCapabilityGuideRow {
            title: "安装 Nginx",
            tone: "neutral",
            reason: capability_reason(
                node.last_nginx_version.as_deref(),
                "启用 Nginx 反向代理切流时，目标节点需要 nginx 命令和 nginx.service。",
            ),
            command: format!(
                "{install_prefix}sudo apt-get update && sudo apt-get install -y nginx && sudo systemctl enable --now nginx"
            ),
            verify: "重新探测后应看到 nginx version，部署确认页不再阻断 Nginx 切流。",
            install_component: "nginx",
            can_install: true,
        });
    }

    if guides.is_empty() {
        guides.push(NodeCapabilityGuideRow {
            title: "节点能力已就绪",
            tone: "success",
            reason: "Docker、Compose、systemd、Caddy 和 Nginx 最近一次探测均可用。".to_owned(),
            command: if node.node_type == "ssh" {
                format!("{install_prefix}docker compose version")
            } else {
                "docker compose version".to_owned()
            },
            verify: "可以继续作为 Compose 或二进制部署目标使用。",
            install_component: "",
            can_install: false,
        });
    }

    guides
}

fn capability_reason(value: Option<&str>, fallback: &'static str) -> String {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        first_line(value).to_owned()
    }
}

fn first_line(value: &str) -> &str {
    value.lines().next().unwrap_or(value).trim()
}

fn node_check_status_label(status: &str) -> &'static str {
    match status {
        "passed" => "通过",
        "failed" => "失败",
        _ => "未知",
    }
}

fn node_check_status_tone(status: &str) -> &'static str {
    match status {
        "passed" => "success",
        "failed" => "warning",
        _ => "neutral",
    }
}

fn node_error_response(err: NodeError) -> Response {
    let status = node_error_status(&err);
    (status, err.message().to_owned()).into_response()
}

fn node_error_status(err: &NodeError) -> StatusCode {
    match err {
        NodeError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        NodeError::Conflict(_) => StatusCode::CONFLICT,
        NodeError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn node_credential_error_response(err: NodeCredentialError) -> Response {
    let status = match err {
        NodeCredentialError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        NodeCredentialError::Conflict(_) => StatusCode::CONFLICT,
        NodeCredentialError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, err.message().to_owned()).into_response()
}

fn credential_status_label(status: &str) -> &'static str {
    match status {
        "active" => "启用",
        "disabled" => "禁用",
        _ => "未知",
    }
}

fn credential_status_tone(status: &str) -> &'static str {
    match status {
        "active" => "success",
        "disabled" => "neutral",
        _ => "warning",
    }
}

fn credential_status_toggle_value(status: &str) -> &'static str {
    if status == "disabled" {
        "active"
    } else {
        "disabled"
    }
}

fn credential_status_toggle_label(status: &str) -> &'static str {
    if status == "disabled" {
        "启用凭据"
    } else {
        "禁用凭据"
    }
}

fn app_type_label(app_type: &str) -> &'static str {
    match app_type {
        "binary" => "二进制",
        _ => "Docker Compose",
    }
}

fn deploy_strategy_label(strategy: &str) -> &'static str {
    match strategy {
        "rolling_continue" => "逐节点继续，最终汇总失败",
        _ => "滚动部署，失败停止",
    }
}

fn binary_release_strategy_label(strategy: &str) -> &'static str {
    match strategy {
        "blue_green" => "Blue/Green",
        _ => "systemd restart",
    }
}

fn binary_proxy_kind_label(kind: &str) -> &'static str {
    match kind {
        "caddy" => "Caddy",
        "nginx" => "Nginx",
        _ => "未启用",
    }
}

fn binary_standby_slot(active_slot: &str) -> &'static str {
    match active_slot {
        "green" => "blue",
        _ => "green",
    }
}

fn app_status_label(status: &str) -> &'static str {
    match status {
        "ready" => "待部署",
        "deploying" => "部署中",
        "running" => "运行中",
        "failed" => "失败",
        "disabled" => "已停用",
        _ => "草稿",
    }
}

fn app_status_tone(status: &str) -> &'static str {
    match status {
        "running" => "success",
        "deploying" => "active",
        "failed" | "disabled" => "warning",
        _ => "neutral",
    }
}

fn app_status_toggle_value(status: &str) -> &'static str {
    if status == "disabled" {
        "ready"
    } else {
        "disabled"
    }
}

fn app_status_toggle_label(status: &str) -> &'static str {
    if status == "disabled" {
        "启用"
    } else {
        "停用"
    }
}

fn account_status_label(status: &str) -> &'static str {
    match status {
        "active" => "启用",
        "locked" => "锁定",
        _ => "禁用",
    }
}

fn account_status_tone(status: &str) -> &'static str {
    match status {
        "active" => "success",
        "locked" => "warning",
        _ => "neutral",
    }
}

fn account_status_toggle_value(status: &str) -> &'static str {
    if status == "active" {
        "disabled"
    } else {
        "active"
    }
}

fn account_status_toggle_label(status: &str) -> &'static str {
    match status {
        "active" => "禁用",
        "locked" => "解锁",
        _ => "启用",
    }
}

fn account_security_view(account: &crate::auth::AccountListItem) -> (String, &'static str) {
    if account.status == "locked" {
        let reason = if account.locked_reason.is_empty() {
            "登录失败次数过多"
        } else {
            &account.locked_reason
        };
        let locked_at = account.locked_at.as_deref().unwrap_or("未知时间");
        return (format!("{reason}，锁定于 {locked_at}"), "warning");
    }
    if account.failed_login_attempts > 0 {
        return (
            format!(
                "连续登录失败 {} 次，成功登录后自动清零",
                account.failed_login_attempts
            ),
            "warning",
        );
    }
    ("正常".to_owned(), "success")
}

fn normalize_rbac_filter(value: Option<&str>) -> &str {
    value.unwrap_or_default().trim()
}

fn account_summary_items(accounts: &[crate::auth::AccountListItem]) -> Vec<SummaryItem> {
    let active = accounts
        .iter()
        .filter(|account| account.status == "active")
        .count();
    let locked = accounts
        .iter()
        .filter(|account| account.status == "locked")
        .count();
    let sessions = accounts
        .iter()
        .map(|account| account.active_session_count)
        .sum::<i64>();
    vec![
        SummaryItem {
            label: "账号总数",
            value: accounts.len().to_string(),
            detail: format!(
                "{active} 个启用，{} 个停用",
                accounts.len().saturating_sub(active)
            ),
            tone: "neutral",
        },
        SummaryItem {
            label: "活跃会话",
            value: sessions.to_string(),
            detail: "当前仍可续期的后台登录会话".to_owned(),
            tone: if sessions > 0 { "active" } else { "neutral" },
        },
        SummaryItem {
            label: "安全提醒",
            value: locked.to_string(),
            detail: "锁定账号需要管理员解锁后才能登录".to_owned(),
            tone: if locked > 0 { "warning" } else { "success" },
        },
        SummaryItem {
            label: "角色池",
            value: accounts
                .iter()
                .filter(|account| {
                    account
                        .role_ids
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
                })
                .count()
                .to_string(),
            detail: "已至少分配一个角色的账号数".to_owned(),
            tone: "neutral",
        },
    ]
}

fn account_status_filter_rows(selected: &str) -> Vec<RbacFilterOptionRow> {
    [
        ("", "全部状态"),
        ("active", "启用"),
        ("disabled", "禁用"),
        ("locked", "锁定"),
    ]
    .into_iter()
    .map(|(value, label)| RbacFilterOptionRow {
        value: value.to_owned(),
        label: label.to_owned(),
        selected: value == selected,
    })
    .collect()
}

fn account_role_filter_rows(
    roles: &[crate::auth::RoleOption],
    selected: &str,
) -> Vec<RbacFilterOptionRow> {
    let mut rows = vec![RbacFilterOptionRow {
        value: String::new(),
        label: "全部角色".to_owned(),
        selected: selected.is_empty(),
    }];
    rows.extend(roles.iter().map(|role| RbacFilterOptionRow {
        value: role.id.to_string(),
        label: format!("{} ({})", role.role_name, role.role_code),
        selected: selected == role.id.to_string(),
    }));
    rows
}

fn account_matches_filter(
    account: &crate::auth::AccountListItem,
    status: &str,
    role: &str,
    query: &str,
) -> bool {
    if !status.is_empty() && account.status != status {
        return false;
    }
    if !role.is_empty() {
        let role_id = role.parse::<i64>().ok();
        let assigned_role_ids = parse_id_csv(account.role_ids.as_deref());
        if role_id.is_none_or(|role_id| !assigned_role_ids.contains(&role_id)) {
            return false;
        }
    }
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    account.username.to_ascii_lowercase().contains(&query)
        || account.display_name.to_ascii_lowercase().contains(&query)
        || account
            .role_names
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains(&query)
}

fn role_summary_items(
    roles: &[crate::auth::RoleListItem],
    total_permission_count: usize,
) -> Vec<SummaryItem> {
    let active = roles.iter().filter(|role| role.status == "active").count();
    let custom = roles.iter().filter(|role| role.is_system == 0).count();
    let highest_coverage = roles
        .iter()
        .map(|role| {
            permission_coverage_percent(role.permission_count as usize, total_permission_count)
        })
        .max()
        .unwrap_or(0);
    vec![
        SummaryItem {
            label: "角色总数",
            value: roles.len().to_string(),
            detail: format!("{active} 个启用，{custom} 个自定义"),
            tone: "neutral",
        },
        SummaryItem {
            label: "权限项",
            value: total_permission_count.to_string(),
            detail: "按模块分组展示页面权限和操作权限".to_owned(),
            tone: "active",
        },
        SummaryItem {
            label: "最高覆盖",
            value: format!("{highest_coverage}%"),
            detail: "单个角色覆盖全部权限项的最高比例".to_owned(),
            tone: if highest_coverage >= 80 {
                "warning"
            } else {
                "neutral"
            },
        },
        SummaryItem {
            label: "内置策略",
            value: roles
                .iter()
                .filter(|role| role.is_system == 1)
                .count()
                .to_string(),
            detail: "系统内置角色随版本同步，不在页面直接改写".to_owned(),
            tone: "success",
        },
    ]
}

fn role_status_filter_rows(selected: &str) -> Vec<RbacFilterOptionRow> {
    [("", "全部状态"), ("active", "启用"), ("disabled", "禁用")]
        .into_iter()
        .map(|(value, label)| RbacFilterOptionRow {
            value: value.to_owned(),
            label: label.to_owned(),
            selected: value == selected,
        })
        .collect()
}

fn role_module_filter_rows(
    groups: &[PermissionGroup<'_>],
    selected: &str,
) -> Vec<RbacFilterOptionRow> {
    let mut rows = vec![RbacFilterOptionRow {
        value: String::new(),
        label: "全部模块".to_owned(),
        selected: selected.is_empty(),
    }];
    rows.extend(groups.iter().map(|group| RbacFilterOptionRow {
        value: group.module.to_owned(),
        label: group.module.to_owned(),
        selected: selected == group.module,
    }));
    rows
}

fn permission_group_rows(
    groups: &std::collections::BTreeMap<String, Vec<crate::auth::PermissionView>>,
) -> Vec<PermissionGroup<'_>> {
    groups
        .iter()
        .map(|(module, permissions)| PermissionGroup {
            id: module,
            module,
            permissions: permissions
                .iter()
                .map(|permission| PermissionRow {
                    id: permission.id,
                    key: &permission.permission_key,
                    name: &permission.permission_name,
                    description: &permission.description,
                    resource_type: &permission.resource_type,
                    resource_tone: permission_resource_tone(&permission.resource_type),
                })
                .collect(),
        })
        .collect()
}

fn permission_dependencies_json(groups: &[PermissionGroup<'_>]) -> String {
    let mut entries = Vec::new();
    let known_keys = groups
        .iter()
        .flat_map(|group| group.permissions.iter().map(|permission| permission.key))
        .collect::<std::collections::BTreeSet<_>>();
    for permission in groups
        .iter()
        .flat_map(|group| group.permissions.iter())
        .filter(|permission| permission.resource_type == "action")
    {
        let dependencies = permission_dependencies(permission.key)
            .iter()
            .copied()
            .filter(|key| known_keys.contains(key))
            .collect::<Vec<_>>();
        if dependencies.is_empty() {
            continue;
        }
        let dependency_json = dependencies
            .iter()
            .map(|key| format!("\"{}\"", json_escape(key)))
            .collect::<Vec<_>>()
            .join(",");
        entries.push(format!(
            "\"{}\":[{}]",
            json_escape(permission.key),
            dependency_json
        ));
    }
    format!("{{{}}}", entries.join(","))
}

fn permission_summary_items(groups: &[PermissionGroup<'_>]) -> Vec<SummaryItem> {
    let total_permissions = groups
        .iter()
        .map(|group| group.permissions.len())
        .sum::<usize>();
    let page_permissions = groups
        .iter()
        .flat_map(|group| group.permissions.iter())
        .filter(|permission| permission.resource_type == "page")
        .count();
    let action_permissions = total_permissions.saturating_sub(page_permissions);
    vec![
        SummaryItem {
            label: "权限总数",
            value: total_permissions.to_string(),
            detail: "平台版本注册的页面权限和操作权限".to_owned(),
            tone: "active",
        },
        SummaryItem {
            label: "页面权限",
            value: page_permissions.to_string(),
            detail: "控制导航入口和页面访问".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "操作权限",
            value: action_permissions.to_string(),
            detail: "控制按钮、表单提交和部署动作".to_owned(),
            tone: "warning",
        },
        SummaryItem {
            label: "模块",
            value: groups.len().to_string(),
            detail: "按业务模块组织权限清单".to_owned(),
            tone: "success",
        },
    ]
}

fn normalize_permission_type_filter(value: Option<&str>) -> &str {
    match value.unwrap_or_default().trim() {
        "page" => "page",
        "action" => "action",
        _ => "",
    }
}

fn permission_type_filter_rows(selected: &str) -> Vec<RbacFilterOptionRow> {
    [
        ("", "全部类型"),
        ("page", "页面权限"),
        ("action", "操作权限"),
    ]
    .into_iter()
    .map(|(value, label)| RbacFilterOptionRow {
        value: value.to_owned(),
        label: label.to_owned(),
        selected: value == selected,
    })
    .collect()
}

fn permission_matches_filter(
    permission: &PermissionRow<'_>,
    module: &str,
    selected_module: &str,
    resource_type: &str,
    query: &str,
) -> bool {
    if !selected_module.is_empty() && module != selected_module {
        return false;
    }
    if !resource_type.is_empty() && permission.resource_type != resource_type {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    permission.key.to_ascii_lowercase().contains(&query)
        || permission.name.to_ascii_lowercase().contains(&query)
        || permission.description.to_ascii_lowercase().contains(&query)
}

fn role_matches_filter(
    role: &crate::auth::RoleListItem,
    groups: &[PermissionGroup<'_>],
    status: &str,
    module: &str,
    query: &str,
) -> bool {
    if !status.is_empty() && role.status != status {
        return false;
    }
    let assigned_permission_ids = parse_id_csv(role.permission_ids.as_deref());
    if !module.is_empty()
        && !groups.iter().any(|group| {
            group.module == module
                && group
                    .permissions
                    .iter()
                    .any(|permission| assigned_permission_ids.contains(&permission.id))
        })
    {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    role.role_code.to_ascii_lowercase().contains(&query)
        || role.role_name.to_ascii_lowercase().contains(&query)
        || role.description.to_ascii_lowercase().contains(&query)
}

fn role_action_permission_count(
    groups: &[PermissionGroup<'_>],
    assigned_permission_ids: &[i64],
) -> usize {
    groups
        .iter()
        .flat_map(|group| group.permissions.iter())
        .filter(|permission| {
            permission.resource_type == "action" && assigned_permission_ids.contains(&permission.id)
        })
        .count()
}

fn permission_coverage_percent(selected: usize, total: usize) -> usize {
    selected.saturating_mul(100).checked_div(total).unwrap_or(0)
}

fn permission_resource_tone(resource_type: &str) -> &'static str {
    if resource_type == "action" {
        "active"
    } else {
        "neutral"
    }
}

fn session_summary_items(sessions: &[crate::auth::SessionListItem]) -> Vec<SummaryItem> {
    let active = sessions
        .iter()
        .filter(|session| session.session_status == "active")
        .count();
    let external = sessions
        .iter()
        .filter(|session| session.last_ip != "127.0.0.1" && session.last_ip != "::1")
        .count();
    vec![
        SummaryItem {
            label: "会话记录",
            value: sessions.len().to_string(),
            detail: "最近 100 条后台登录会话".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "活跃会话",
            value: active.to_string(),
            detail: "可继续通过 Refresh Token 续期".to_owned(),
            tone: if active > 0 { "active" } else { "neutral" },
        },
        SummaryItem {
            label: "外部来源",
            value: external.to_string(),
            detail: "非本机 IP 的登录来源需要关注".to_owned(),
            tone: if external > 0 { "warning" } else { "success" },
        },
        SummaryItem {
            label: "已失效",
            value: sessions.len().saturating_sub(active).to_string(),
            detail: "退出、轮换、强制下线或过期后的会话".to_owned(),
            tone: "neutral",
        },
    ]
}

fn session_status_filter_rows(selected: &str) -> Vec<RbacFilterOptionRow> {
    [("", "全部状态"), ("active", "活跃"), ("revoked", "已失效")]
        .into_iter()
        .map(|(value, label)| RbacFilterOptionRow {
            value: value.to_owned(),
            label: label.to_owned(),
            selected: value == selected,
        })
        .collect()
}

fn api_token_summary_items(tokens: &[crate::auth::ApiTokenListItem]) -> Vec<SummaryItem> {
    let active = tokens
        .iter()
        .filter(|token| token.status == "active")
        .count();
    let used = tokens
        .iter()
        .filter(|token| token.last_used_at.is_some())
        .count();
    vec![
        SummaryItem {
            label: "Token",
            value: tokens.len().to_string(),
            detail: "recent 100 api tokens".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "Active",
            value: active.to_string(),
            detail: "tokens allowed to call /api/v1".to_owned(),
            tone: if active > 0 { "active" } else { "neutral" },
        },
        SummaryItem {
            label: "Used",
            value: used.to_string(),
            detail: "tokens with last-used timestamp".to_owned(),
            tone: "success",
        },
        SummaryItem {
            label: "Revoked",
            value: tokens.len().saturating_sub(active).to_string(),
            detail: "tokens no longer accepted".to_owned(),
            tone: "warning",
        },
    ]
}

fn api_token_status_label(status: &str) -> &'static str {
    match status {
        "active" => "可用",
        "revoked" => "已吊销",
        _ => "未知",
    }
}

fn api_token_status_tone(status: &str) -> &'static str {
    match status {
        "active" => "success",
        "revoked" => "warning",
        _ => "neutral",
    }
}

fn session_matches_filter(
    session: &crate::auth::SessionListItem,
    status: &str,
    query: &str,
) -> bool {
    if !status.is_empty() {
        if status == "revoked" {
            if session.session_status == "active" {
                return false;
            }
        } else if session.session_status != status {
            return false;
        }
    }
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    session.username.to_ascii_lowercase().contains(&query)
        || session.display_name.to_ascii_lowercase().contains(&query)
        || session.last_ip.to_ascii_lowercase().contains(&query)
        || session.user_agent.to_ascii_lowercase().contains(&query)
}

fn session_risk_view(session: &crate::auth::SessionListItem) -> (&'static str, &'static str) {
    if session.session_status != "active" {
        return ("已失效", "neutral");
    }
    if session.last_ip.is_empty() {
        return ("来源未知", "warning");
    }
    if session.last_ip == "127.0.0.1" || session.last_ip == "::1" {
        return ("本机", "success");
    }
    ("外部来源", "warning")
}

fn deploy_action_label(action: &str) -> &'static str {
    match action {
        "compose_up" => "部署",
        "compose_down" => "停止",
        "compose_restart" => "重启",
        "binary_restart" => "重启二进制",
        "binary_stop" => "停止二进制",
        _ => "操作",
    }
}

fn parse_compose_confirm_action(action: &str) -> Option<ComposeTaskAction> {
    match action {
        "up" => Some(ComposeTaskAction::Up),
        "down" => Some(ComposeTaskAction::Down),
        "restart" => Some(ComposeTaskAction::Restart),
        _ => None,
    }
}

fn parse_binary_confirm_action(action: &str) -> Option<BinaryTaskAction> {
    match action {
        "restart" => Some(BinaryTaskAction::Restart),
        "stop" => Some(BinaryTaskAction::Stop),
        _ => None,
    }
}

fn compose_action_segment(action: ComposeTaskAction) -> &'static str {
    match action {
        ComposeTaskAction::Up => "up",
        ComposeTaskAction::Down => "down",
        ComposeTaskAction::Restart => "restart",
    }
}

fn binary_action_segment(action: BinaryTaskAction) -> &'static str {
    match action {
        BinaryTaskAction::Restart => "restart",
        BinaryTaskAction::Stop => "stop",
    }
}

fn compose_submit_path(app_id: i64, action: ComposeTaskAction) -> String {
    format!("/apps/{app_id}/compose/{}", compose_action_segment(action))
}

fn binary_submit_path(app_id: i64, action: BinaryTaskAction) -> String {
    format!("/apps/{app_id}/binary/{}", binary_action_segment(action))
}

fn compose_confirm_path(app_id: i64, action: ComposeTaskAction) -> String {
    format!("{}/confirm", compose_submit_path(app_id, action))
}

fn binary_confirm_path(app_id: i64, action: BinaryTaskAction) -> String {
    format!("{}/confirm", binary_submit_path(app_id, action))
}

fn deploy_confirm_action_label(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => "部署",
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => "停止",
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => "重启",
        DeployConfirmAction::Binary(BinaryTaskAction::Restart) => "重启二进制",
        DeployConfirmAction::Binary(BinaryTaskAction::Stop) => "停止二进制",
    }
}

fn deploy_confirm_action_tone(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up)
        | DeployConfirmAction::Compose(ComposeTaskAction::Restart)
        | DeployConfirmAction::Binary(BinaryTaskAction::Restart) => "active",
        DeployConfirmAction::Compose(ComposeTaskAction::Down)
        | DeployConfirmAction::Binary(BinaryTaskAction::Stop) => "warning",
    }
}

fn deploy_confirm_action_description(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => {
            "确认目标节点、配置差异和健康检查后，提交 Docker Compose 部署任务。"
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => {
            "确认目标节点后，提交 Docker Compose 停止任务。"
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => {
            "确认目标节点、配置差异和健康检查后，提交 Docker Compose 重启任务。"
        }
        DeployConfirmAction::Binary(BinaryTaskAction::Restart) => {
            "确认目标节点、制品配置和健康检查后，提交二进制 systemd 重启任务。"
        }
        DeployConfirmAction::Binary(BinaryTaskAction::Stop) => {
            "确认目标节点后，提交二进制 systemd 停止任务。"
        }
    }
}

fn deployment_status_label(status: &str) -> &'static str {
    match status {
        "success" => "成功",
        "failed" => "失败",
        "running" => "执行中",
        _ => "未知",
    }
}

fn deployment_status_tone(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "running" => "active",
        "failed" => "warning",
        _ => "neutral",
    }
}

fn snapshot_kind_label(kind: &str) -> &'static str {
    match kind {
        "initial" => "初始配置",
        "deploy" => "部署快照",
        "manual" => "手动保存",
        _ => "配置快照",
    }
}

fn deploy_diff_view(diff: &crate::apps::AppDeployDiff) -> AppDeployDiffView {
    let changed_labels = diff
        .rows
        .iter()
        .filter(|row| row.changed)
        .map(|row| row.label)
        .collect::<Vec<_>>();
    let changed_count = changed_labels.len();
    let changed_label_text = if changed_labels.is_empty() {
        "无".to_owned()
    } else {
        changed_labels.join("、")
    };
    let rows = diff
        .rows
        .iter()
        .map(|row| AppDeployDiffRow {
            label: row.label,
            current_summary: row.current_summary.clone(),
            baseline_summary: row.baseline_summary.clone(),
            current_preview: row.current_preview.clone(),
            baseline_preview: row.baseline_preview.clone(),
            has_detail: row.changed,
            status: if row.changed { "有变化" } else { "一致" },
            status_tone: if row.changed { "warning" } else { "success" },
        })
        .collect::<Vec<_>>();
    match diff.status {
        AppDeployDiffStatus::NoBaseline => AppDeployDiffView {
            status: "无部署基线",
            status_tone: "neutral",
            baseline: "尚未成功部署".to_owned(),
            risk_title: "首次部署".to_owned(),
            risk_detail: "没有上次成功部署快照可对比；请重点确认目标节点、部署目录和健康检查。"
                .to_owned(),
            changed_count,
            empty_title: "暂无部署基线",
            empty_message: "首次成功部署后，这里会显示当前配置与上次部署快照的差异。",
            rows,
        },
        AppDeployDiffStatus::Unchanged => AppDeployDiffView {
            status: "配置一致",
            status_tone: "success",
            baseline: deploy_diff_baseline_text(diff),
            risk_title: "低风险变更".to_owned(),
            risk_detail: "当前配置与上次成功部署快照一致，提交后仍会执行预检和健康检查。"
                .to_owned(),
            changed_count,
            empty_title: "配置未变化",
            empty_message: "当前运行配置与上次成功部署快照一致。",
            rows,
        },
        AppDeployDiffStatus::Changed => AppDeployDiffView {
            status: "发现配置变化",
            status_tone: "warning",
            baseline: deploy_diff_baseline_text(diff),
            risk_title: format!("{changed_count} 项配置将变更"),
            risk_detail: format!(
                "变更项：{changed_label_text}。提交前请确认端口、环境变量、发布物和启动参数是否符合预期。"
            ),
            changed_count,
            empty_title: "配置未变化",
            empty_message: "当前运行配置与上次成功部署快照一致。",
            rows,
        },
    }
}

fn deploy_diff_baseline_text(diff: &crate::apps::AppDeployDiff) -> String {
    match (
        diff.baseline_snapshot_id,
        diff.baseline_created_at.as_deref(),
    ) {
        (Some(id), Some(created_at)) => format!("部署快照 #{id} · {created_at}"),
        (Some(id), None) => format!("部署快照 #{id}"),
        _ => "尚未成功部署".to_owned(),
    }
}

fn config_summary(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("空")
        .chars()
        .take(80)
        .collect()
}

fn runtime_status_label(status: &str) -> &'static str {
    match status {
        "healthy" => "健康",
        "unhealthy" => "异常",
        "deploying" => "部署中",
        "stopped" => "已停止",
        _ => "未知",
    }
}

fn runtime_status_tone(status: &str) -> &'static str {
    match status {
        "healthy" => "success",
        "deploying" => "active",
        "unhealthy" => "warning",
        _ => "neutral",
    }
}

struct ServiceHealthOverview {
    status: &'static str,
    tone: &'static str,
    summary: &'static str,
    action_hint: &'static str,
}

fn service_health_overview(
    runtime_status: &str,
    health_kind: &HealthCheckKind,
) -> ServiceHealthOverview {
    if matches!(health_kind, HealthCheckKind::None) {
        return ServiceHealthOverview {
            status: "未启用",
            tone: "neutral",
            summary: "不执行健康检查",
            action_hint: "需要自动验收时，在应用配置中启用健康检查",
        };
    }

    match runtime_status {
        "healthy" => ServiceHealthOverview {
            status: "检查通过",
            tone: "success",
            summary: "最近检查通过",
            action_hint: "运行项当前可继续发布或查看日志",
        },
        "unhealthy" => ServiceHealthOverview {
            status: "检查失败",
            tone: "warning",
            summary: "最近检查失败",
            action_hint: "查看最近任务和节点日志定位异常",
        },
        "deploying" => ServiceHealthOverview {
            status: "等待结果",
            tone: "active",
            summary: "部署中，等待健康检查",
            action_hint: "任务完成后会刷新最近检查结果",
        },
        "stopped" => ServiceHealthOverview {
            status: "已停止",
            tone: "neutral",
            summary: "运行项已停止，暂不检查",
            action_hint: "重新部署或重启后恢复健康检查",
        },
        _ => ServiceHealthOverview {
            status: "未检查",
            tone: "neutral",
            summary: "暂无健康检查结果",
            action_hint: "等待首次部署后产生健康检查结果",
        },
    }
}

fn service_node_link_row(
    node: &ServiceTargetNodeItem,
    href: String,
    return_to: &str,
    active: bool,
) -> ServiceNodeLinkRow {
    let task_status = node.last_task_status.as_deref().unwrap_or_default();
    ServiceNodeLinkRow {
        name: node.name.clone(),
        node_key: node.node_key.clone(),
        href,
        node_href: format!("/nodes/{}", node.id),
        task_href: node
            .last_task_id
            .map(|task_id| task_href_with_return(task_id, return_to))
            .unwrap_or_default(),
        task_id: node.last_task_id.unwrap_or_default(),
        task_return_to: return_to.to_owned(),
        active,
        runtime_status: runtime_status_label(&node.runtime_status),
        runtime_status_tone: runtime_status_tone(&node.runtime_status),
        runtime_summary: service_node_runtime_summary(node),
        task_status: if task_status.is_empty() {
            "最近任务"
        } else {
            task_status_label(task_status)
        },
        task_status_tone: task_status_tone(task_status),
        task_action_label: service_task_action_label(node),
        active_version: task_display_text(&node.active_version, "未部署").to_owned(),
        last_health_at: service_node_last_health_at(node),
        message: service_node_message(node),
        has_task_href: node.last_task_id.is_some(),
        can_retry_task: service_node_can_retry_task(node),
    }
}

fn service_node_can_retry_task(node: &ServiceTargetNodeItem) -> bool {
    let task_status = node.last_task_status.as_deref().unwrap_or_default();
    let task_kind = node.last_task_kind.as_deref().unwrap_or_default();
    task_status == "failed" && (is_compose_task_kind(task_kind) || is_binary_task_kind(task_kind))
}

fn service_task_action_label(node: &ServiceTargetNodeItem) -> &'static str {
    if service_node_can_retry_task(node) {
        "查看并重试"
    } else {
        "最近任务"
    }
}

fn task_href_with_return(task_id: i64, return_to: &str) -> String {
    if return_to.trim().is_empty() {
        format!("/tasks/{task_id}")
    } else {
        format!(
            "/tasks/{task_id}?return_to={}",
            encode_query_value(return_to)
        )
    }
}

fn normalize_page(value: Option<usize>, total_pages: usize) -> usize {
    value.unwrap_or(1).clamp(1, total_pages.max(1))
}

fn app_page_href(app_type: &str, status: &str, query: &str, page: usize) -> String {
    let mut params = Vec::new();
    if !app_type.is_empty() {
        params.push(format!("type={}", encode_query_value(app_type)));
    }
    if !status.is_empty() {
        params.push(format!("status={}", encode_query_value(status)));
    }
    if !query.is_empty() {
        params.push(format!("q={}", encode_query_value(query)));
    }
    if page > 1 {
        params.push(format!("page={page}"));
    }
    if params.is_empty() {
        "/apps".to_owned()
    } else {
        format!("/apps?{}", params.join("&"))
    }
}

fn service_node_runtime_summary(node: &ServiceTargetNodeItem) -> String {
    let service_count = if node.service_count > 0 {
        format!("{} 个运行项", node.service_count)
    } else {
        "未记录运行项数".to_owned()
    };
    format!(
        "{} · {} · 版本 {}",
        runtime_status_label(&node.runtime_status),
        service_count,
        task_display_text(&node.active_version, "未部署")
    )
}

fn service_node_last_health_at(node: &ServiceTargetNodeItem) -> String {
    node.last_deploy_at
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&node.updated_at)
        .to_owned()
}

fn service_node_message(node: &ServiceTargetNodeItem) -> String {
    if node.message.trim().is_empty() {
        "等待首次部署".to_owned()
    } else {
        node.message.clone()
    }
}

fn artifact_status_label(status: &str) -> &'static str {
    match status {
        "active" => "当前",
        "registered" => "可用",
        "disabled" => "已清理",
        _ => "未知",
    }
}

fn artifact_status_tone(status: &str) -> &'static str {
    match status {
        "active" => "success",
        "disabled" => "warning",
        _ => "neutral",
    }
}

fn artifact_kind_label(kind: &str) -> &'static str {
    match kind {
        "tar_gz" => "tar.gz",
        "binary" => "binary",
        _ => "unknown",
    }
}

fn artifact_source_label(source: &str) -> &'static str {
    match source {
        "upload" => "上传",
        "initial" => "登记",
        _ => "登记",
    }
}

fn normalize_artifact_status_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "active" => "active",
        "registered" => "registered",
        "disabled" => "disabled",
        _ => "",
    }
}

fn normalize_artifact_kind_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "binary" => "binary",
        "tar_gz" => "tar_gz",
        _ => "",
    }
}

fn normalize_artifact_source_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "upload" => "upload",
        "initial" => "initial",
        _ => "",
    }
}

fn artifact_matches_filters(
    artifact: &crate::apps::ArtifactListItem,
    selected_status: &str,
    selected_kind: &str,
    selected_source: &str,
    query: &str,
) -> bool {
    if !selected_status.is_empty() && artifact.status != selected_status {
        return false;
    }
    if !selected_kind.is_empty() && artifact.artifact_kind != selected_kind {
        return false;
    }
    if !selected_source.is_empty() && artifact.metadata_value("source") != selected_source {
        return false;
    }
    if query.trim().is_empty() {
        return true;
    }
    artifact_search_text(artifact).contains(&query.to_ascii_lowercase())
}

fn artifact_search_text(artifact: &crate::apps::ArtifactListItem) -> String {
    format!(
        "{} {} {} {} {} {} {} {} {}",
        artifact.app_name,
        artifact.app_key,
        artifact.version,
        artifact.artifact_path,
        artifact.artifact_kind,
        artifact.status,
        artifact.metadata,
        artifact.metadata_value("entry_file"),
        artifact.metadata_value("sha256")
    )
    .to_ascii_lowercase()
}

fn short_hash(value: &str) -> String {
    if value.is_empty() {
        "未记录".to_owned()
    } else {
        value.chars().take(12).collect()
    }
}

fn format_size(value: &str) -> String {
    let Ok(size) = value.parse::<u64>() else {
        return "未记录".to_owned();
    };
    if size >= 1024 * 1024 {
        format!("{:.1} MiB", size as f64 / 1024.0 / 1024.0)
    } else if size >= 1024 {
        format!("{:.1} KiB", size as f64 / 1024.0)
    } else {
        format!("{size} B")
    }
}

fn display_text(value: String, fallback: &'static str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

fn target_work_dir_path(work_dir: &str, relative_path: &str) -> String {
    let normalized_work_dir = work_dir.replace('\\', "/");
    let work_dir = normalized_work_dir.trim_end_matches('/');
    if work_dir.is_empty() {
        relative_path.to_owned()
    } else {
        format!("{work_dir}/{relative_path}")
    }
}

fn binary_unit_env_file_name(unit_name: &str) -> String {
    let stem = unit_name.strip_suffix(".service").unwrap_or(unit_name);
    format!("{stem}.env")
}

fn count_apps(apps: &[crate::apps::AppListItem], status: &str) -> usize {
    apps.iter().filter(|app| app.status == status).count()
}

fn dashboard_services_text(services: &[crate::apps::ServiceListItem], app_id: i64) -> String {
    let names = services
        .iter()
        .filter(|service| service.app_id == app_id)
        .map(|service| service.service_name.as_str())
        .take(4)
        .collect::<Vec<_>>();
    if names.is_empty() {
        "未解析服务".to_owned()
    } else {
        names.join(", ")
    }
}

fn app_error_response(err: AppError) -> Response {
    let status = app_error_status(&err);
    (status, err.message().to_owned()).into_response()
}

fn app_error_status(err: &AppError) -> StatusCode {
    match err {
        AppError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        AppError::Conflict(_) => StatusCode::CONFLICT,
        AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn platform_error_response(err: PlatformConfigError) -> Response {
    let status = platform_error_status(&err);
    (status, err.message().to_owned()).into_response()
}

fn platform_error_status(err: &PlatformConfigError) -> StatusCode {
    match err {
        PlatformConfigError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        PlatformConfigError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn task_status_label(status: &str) -> &'static str {
    match status {
        "queued" => "等待中",
        "running" => "执行中",
        "success" => "成功",
        "failed" => "失败",
        "canceled" => "已取消",
        _ => "未知",
    }
}

fn task_status_tone(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "running" => "active",
        "failed" | "canceled" => "warning",
        _ => "neutral",
    }
}

fn task_phase_label(phase: &str) -> &'static str {
    match phase {
        "queued" => "等待入队",
        "preflight" => "部署前预检",
        "preparing_files" => "准备运行文件",
        "executing" => "执行命令",
        "healthchecking" => "健康检查",
        "completed" => "已完成",
        "failed" => "失败收尾",
        "canceled" => "已取消",
        _ => "未知阶段",
    }
}

fn task_phase_tone(phase: &str) -> &'static str {
    match phase {
        "completed" => "success",
        "preflight" | "preparing_files" | "executing" | "healthchecking" => "active",
        "failed" | "canceled" => "warning",
        _ => "neutral",
    }
}

fn task_phase_detail(phase: &str) -> &'static str {
    match phase {
        "queued" => "任务已创建，正在等待后台队列调度。",
        "preflight" => "正在检查节点状态、Docker/Compose 能力、目录权限和端口风险。",
        "preparing_files" => "正在准备 compose、环境变量、systemd unit、制品或代理配置等运行文件。",
        "executing" => "正在目标节点执行部署、停止、重启、安装或切流命令。",
        "healthchecking" => "命令已执行完成，正在验证服务是否按预期运行。",
        "completed" => "任务已经完成，节点结果和部署记录已写回。",
        "failed" => "任务失败并完成收尾，请查看日志和节点结果定位原因。",
        "canceled" => "任务在开始执行前已取消，不会再进入后台执行。",
        _ => "当前阶段无法识别，请查看任务日志确认执行状态。",
    }
}

fn task_execution_guide_view(
    task: &crate::tasks::TaskDetailItem,
    node_results: &[crate::tasks::TaskNodeResultItem],
) -> TaskExecutionGuideView {
    let success_count = node_results
        .iter()
        .filter(|result| result.status == "success")
        .count();
    let failed_count = node_results
        .iter()
        .filter(|result| result.status == "failed")
        .count();
    let skipped_count = node_results
        .iter()
        .filter(|result| result.status == "skipped")
        .count();
    let node_summary = if node_results.is_empty() {
        "节点结果尚未写入，任务进入目标节点执行后会在这里汇总。".to_owned()
    } else {
        format!(
            "{} 个节点已记录：{} 成功，{} 失败，{} 跳过。",
            node_results.len(),
            success_count,
            failed_count,
            skipped_count
        )
    };
    let title = match task.status.as_str() {
        "queued" => "等待后台队列调度".to_owned(),
        "running" => format!("正在执行：{}", task_phase_label(&task.phase)),
        "success" => "任务已完成".to_owned(),
        "failed" => "任务失败，需要处理".to_owned(),
        "canceled" => "任务已取消".to_owned(),
        _ => "任务状态未知".to_owned(),
    };
    let tone = match task.status.as_str() {
        "success" => "success",
        "running" => "active",
        "failed" | "canceled" => "warning",
        _ => "neutral",
    };
    let detail = match task.status.as_str() {
        "queued" => "后台 worker 尚未开始执行，仍可取消该任务。".to_owned(),
        "running" => task_phase_detail(&task.phase).to_owned(),
        "success" => {
            if failed_count == 0 && skipped_count == 0 {
                task_phase_detail(&task.phase).to_owned()
            } else {
                "任务已结束，请确认节点结果是否符合预期。".to_owned()
            }
        }
        "failed" => {
            if failed_count > 0 {
                "至少一个目标节点失败；优先查看失败节点结果，再查看下方任务日志中的第一条错误。"
                    .to_owned()
            } else {
                "任务在节点结果写入前失败；优先查看任务日志里的预检、命令或系统错误。".to_owned()
            }
        }
        "canceled" => "任务在执行前被取消，没有继续下发到目标节点。".to_owned(),
        _ => "当前状态无法识别，请查看任务日志确认实际执行情况。".to_owned(),
    };
    let log_hint = match task.status.as_str() {
        "failed" => "先看失败摘要，再按时间顺序定位第一条错误日志。",
        "running" => "页面会自动刷新，日志会按执行顺序继续追加。",
        "queued" => "任务开始前通常只有入队日志。",
        _ => "日志保留预检、命令输出和收尾信息。",
    };
    let next_step = match task.status.as_str() {
        "queued" => "如果不想继续执行，可以点击右上角取消。".to_owned(),
        "running" => "等待当前阶段完成；如果卡住，查看最新日志和目标节点连接状态。".to_owned(),
        "success" => "可以返回应用详情查看运行状态、运行项日志和部署历史。".to_owned(),
        "failed" if is_retryable_task_kind(&task.task_kind) => {
            "修复配置、节点能力或运行环境后，可以在右上角重试该任务。".to_owned()
        }
        "failed" => "修复失败原因后，从对应页面重新发起操作。".to_owned(),
        "canceled" => "需要执行时，请回到应用或节点页面重新发起操作。".to_owned(),
        _ => "继续查看日志和元信息确认任务状态。".to_owned(),
    };
    TaskExecutionGuideView {
        title,
        tone,
        detail,
        node_summary,
        log_hint,
        next_step,
    }
}

struct QueueState {
    label: String,
    tone: &'static str,
}

fn task_queue_summary(tasks: &[crate::tasks::TaskListItem]) -> String {
    let running = tasks.iter().filter(|task| task.status == "running").count();
    let queued = tasks.iter().filter(|task| task.status == "queued").count();
    if running == 0 && queued == 0 {
        "当前没有等待或执行中的部署任务。".to_owned()
    } else {
        format!("{running} 个执行中，{queued} 个等待中。")
    }
}

fn task_queue_state(
    status: &str,
    task_id: i64,
    tasks: &[crate::tasks::TaskListItem],
) -> QueueState {
    match status {
        "running" => QueueState {
            label: "正在执行".to_owned(),
            tone: "active",
        },
        "queued" => {
            let queued_before = tasks
                .iter()
                .filter(|task| task.status == "queued" && task.id < task_id)
                .count();
            QueueState {
                label: format!("队列第 {} 位", queued_before + 1),
                tone: "neutral",
            }
        }
        _ => QueueState {
            label: "不在队列".to_owned(),
            tone: "neutral",
        },
    }
}

fn task_detail_queue_state(
    status: &str,
    position: &crate::tasks::TaskQueuePositionItem,
) -> QueueState {
    match status {
        "running" => QueueState {
            label: format!("正在执行，前方仍有 {} 个运行任务", position.running_before),
            tone: "active",
        },
        "queued" => QueueState {
            label: format!(
                "队列第 {} 位，前方 {} 个运行中、{} 个等待中",
                position.queued_before + 1,
                position.running_before,
                position.queued_before
            ),
            tone: "neutral",
        },
        _ => QueueState {
            label: "不在队列".to_owned(),
            tone: "neutral",
        },
    }
}

fn task_node_result_status_label(status: &str) -> &'static str {
    match status {
        "success" => "成功",
        "failed" => "失败",
        "skipped" => "已跳过",
        _ => "未知",
    }
}

fn task_node_result_status_tone(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "failed" | "skipped" => "warning",
        _ => "neutral",
    }
}

struct TaskNodeResultAction {
    kind: &'static str,
    label: &'static str,
    component: &'static str,
    hint: &'static str,
}

impl TaskNodeResultAction {
    fn none() -> Self {
        Self {
            kind: "",
            label: "",
            component: "",
            hint: "",
        }
    }

    fn has_action(&self) -> bool {
        !self.kind.is_empty()
    }
}

fn task_node_result_action(result: &crate::tasks::TaskNodeResultItem) -> TaskNodeResultAction {
    if result.status != "failed" {
        return TaskNodeResultAction::none();
    }
    let message = result.message.to_ascii_lowercase();
    if message_mentions_component_issue(&message, &["docker compose", "compose"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Compose",
            component: "compose",
            hint: "安装 Docker Compose 插件后重新探测节点，再回到任务或部署确认页。",
        };
    }
    if message_mentions_component_issue(&message, &["docker daemon", "docker engine", "docker"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Docker",
            component: "docker",
            hint: "安装 Docker Engine 后重新探测节点，再重试部署任务。",
        };
    }
    if message_mentions_component_issue(&message, &["caddy"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Caddy",
            component: "caddy",
            hint: "安装 Caddy 后重新探测节点，适用于 Blue/Green 反向代理切流。",
        };
    }
    if message_mentions_component_issue(&message, &["nginx"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Nginx",
            component: "nginx",
            hint: "安装 Nginx 后重新探测节点，适用于 Nginx 反向代理切流。",
        };
    }
    if message.contains("离线")
        || message.contains("offline")
        || message.contains("unknown")
        || message.contains("未探测")
    {
        return TaskNodeResultAction {
            kind: "check",
            label: "重新探测",
            component: "",
            hint: "重新探测会刷新节点在线状态和组件能力。",
        };
    }
    TaskNodeResultAction {
        kind: "detail",
        label: "查看节点",
        component: "",
        hint: "查看节点最近探测结果、组件能力和关联任务。",
    }
}

fn message_mentions_component_issue(message: &str, components: &[&str]) -> bool {
    let has_component = components
        .iter()
        .any(|component| message.contains(component));
    has_component
        && [
            "未通过",
            "不可用",
            "未安装",
            "找不到",
            "不存在",
            "not found",
            "command not found",
            "no such file",
            "is not installed",
            "cannot connect",
            "能力探测",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

fn normalize_app_type_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "compose" => "compose",
        "binary" => "binary",
        _ => "",
    }
}

fn normalize_app_status_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "draft" => "draft",
        "running" => "running",
        "deploying" => "deploying",
        "failed" => "failed",
        "stopped" => "stopped",
        "disabled" => "disabled",
        _ => "",
    }
}

fn app_matches_filters(
    app: &crate::apps::AppListItem,
    selected_type: &str,
    selected_status: &str,
    query: &str,
) -> bool {
    if !selected_type.is_empty() && app.app_type != selected_type {
        return false;
    }
    if !selected_status.is_empty() && app.status != selected_status {
        return false;
    }
    if query.trim().is_empty() {
        return true;
    }
    app_search_text(app).contains(&query.to_ascii_lowercase())
}

fn app_search_text(app: &crate::apps::AppListItem) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        app.name,
        app.app_key,
        app.description,
        app.app_type,
        app.deploy_strategy,
        app.work_dir,
        app.target_names.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase()
}

fn normalize_node_type_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "local" => "local",
        "ssh" => "ssh",
        _ => "",
    }
}

fn normalize_node_status_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "online" => "online",
        "offline" => "offline",
        "disabled" => "disabled",
        _ => "",
    }
}

fn node_matches_filters(
    node: &crate::nodes::NodeListItem,
    selected_type: &str,
    selected_status: &str,
    query: &str,
) -> bool {
    if !selected_type.is_empty() && node.node_type != selected_type {
        return false;
    }
    if !selected_status.is_empty() && node.status != selected_status {
        return false;
    }
    if query.trim().is_empty() {
        return true;
    }
    node_search_text(node).contains(&query.to_ascii_lowercase())
}

fn node_search_text(node: &crate::nodes::NodeListItem) -> String {
    format!(
        "{} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        node.name,
        node.node_key,
        node.node_type,
        node.address,
        node.ssh_port,
        node.ssh_user,
        node.work_dir,
        node.region,
        node.labels,
        node.docker_status,
        node.capability_status,
        node.last_message.as_deref().unwrap_or_default(),
        node.last_os_info.as_deref().unwrap_or_default(),
        node.last_disk_info.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase()
}

fn normalize_service_kind_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "compose" => "compose",
        "binary" => "binary",
        _ => "",
    }
}

fn normalize_service_status_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "healthy" => "healthy",
        "unhealthy" => "unhealthy",
        "deploying" => "deploying",
        "stopped" => "stopped",
        "unknown" => "unknown",
        _ => "",
    }
}

fn service_matches_filters(
    service: &crate::apps::ServiceListItem,
    selected_kind: &str,
    selected_status: &str,
    query: &str,
) -> bool {
    if !selected_kind.is_empty()
        && !service
            .service_kind
            .to_ascii_lowercase()
            .contains(selected_kind)
    {
        return false;
    }
    if !selected_status.is_empty() && service.runtime_status != selected_status {
        return false;
    }
    if query.trim().is_empty() {
        return true;
    }
    service_search_text(service).contains(&query.to_ascii_lowercase())
}

fn service_search_text(service: &crate::apps::ServiceListItem) -> String {
    let node_text = service
        .target_nodes
        .iter()
        .map(|node| format!("{} {}", node.name, node.node_key))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{} {} {} {} {} {} {} {} {} {} {}",
        service.service_name,
        service.app_name,
        service.app_key,
        service.service_kind,
        service.image,
        service.ports,
        service.replicas,
        service.target_names,
        service.runtime_summary,
        service.active_version,
        node_text
    )
    .to_ascii_lowercase()
}

fn task_status_filter_rows(
    selected_status: &str,
    counts: &[crate::tasks::TaskStatusCount],
) -> Vec<TaskFilterOptionRow> {
    let total = counts.iter().map(|item| item.count).sum::<i64>();
    let mut rows = vec![TaskFilterOptionRow {
        value: String::new(),
        label: "全部状态",
        count: total,
        selected: selected_status.is_empty(),
    }];
    for status in ["queued", "running", "success", "failed", "canceled"] {
        rows.push(TaskFilterOptionRow {
            value: status.to_owned(),
            label: task_status_label(status),
            count: counts
                .iter()
                .find(|item| item.status == status)
                .map(|item| item.count)
                .unwrap_or(0),
            selected: selected_status == status,
        });
    }
    rows
}

fn task_phase_filter_rows(selected_phase: &str) -> Vec<TaskFilterOptionRow> {
    let mut rows = vec![TaskFilterOptionRow {
        value: String::new(),
        label: "全部阶段",
        count: 0,
        selected: selected_phase.is_empty(),
    }];
    for phase in [
        "queued",
        "preflight",
        "preparing_files",
        "executing",
        "healthchecking",
        "completed",
        "failed",
        "canceled",
    ] {
        rows.push(TaskFilterOptionRow {
            value: phase.to_owned(),
            label: task_phase_label(phase),
            count: 0,
            selected: selected_phase == phase,
        });
    }
    rows
}

fn task_kind_label(task_kind: &str) -> &'static str {
    match task_kind {
        "compose.up" => "Compose 部署",
        "compose.down" => "Compose 停止",
        "compose.restart" => "Compose 重启",
        "binary.restart" => "二进制重启",
        "binary.stop" => "二进制停止",
        "node.install.docker" => "安装 Docker",
        "node.install.compose" => "安装 Compose",
        "node.install.caddy" => "安装 Caddy",
        "node.install.nginx" => "安装 Nginx",
        _ => "操作任务",
    }
}

fn task_phase_step_rows(current_phase: &str) -> Vec<TaskPhaseStepRow> {
    let phases = [
        ("queued", "等待入队"),
        ("preflight", "部署前预检"),
        ("preparing_files", "准备运行文件"),
        ("executing", "执行命令"),
        ("healthchecking", "健康检查"),
        ("completed", "已完成"),
        ("failed", "失败收尾"),
        ("canceled", "已取消"),
    ];
    let current_index = phases
        .iter()
        .position(|(phase, _)| *phase == current_phase)
        .unwrap_or(0);
    let terminal = matches!(current_phase, "completed" | "failed" | "canceled");
    phases
        .iter()
        .enumerate()
        .map(|(index, (_, label))| {
            let (state, tone) = if index < current_index {
                ("已完成", "success")
            } else if index == current_index {
                if terminal {
                    ("当前结果", task_phase_terminal_tone(current_phase))
                } else {
                    ("当前阶段", "active")
                }
            } else {
                ("待执行", "neutral")
            };
            TaskPhaseStepRow { label, state, tone }
        })
        .collect()
}

fn task_phase_terminal_tone(phase: &str) -> &'static str {
    match phase {
        "completed" => "success",
        "failed" | "canceled" => "warning",
        _ => "active",
    }
}

fn task_kind_filter_rows(selected_task_kind: &str) -> Vec<TaskFilterOptionRow> {
    let mut rows = vec![TaskFilterOptionRow {
        value: String::new(),
        label: "全部类型",
        count: 0,
        selected: selected_task_kind.is_empty(),
    }];
    for (task_kind, label) in [
        ("compose.up", "Compose 部署"),
        ("compose.down", "Compose 停止"),
        ("compose.restart", "Compose 重启"),
        ("binary.restart", "二进制重启"),
        ("binary.stop", "二进制停止"),
        ("node.install.docker", "安装 Docker"),
        ("node.install.compose", "安装 Compose"),
        ("node.install.caddy", "安装 Caddy"),
        ("node.install.nginx", "安装 Nginx"),
    ] {
        rows.push(TaskFilterOptionRow {
            value: task_kind.to_owned(),
            label,
            count: 0,
            selected: selected_task_kind == task_kind,
        });
    }
    rows
}

fn is_retryable_task_kind(task_kind: &str) -> bool {
    matches!(
        task_kind,
        "compose.up" | "compose.down" | "compose.restart" | "binary.restart" | "binary.stop"
    )
}

fn is_compose_task_kind(task_kind: &str) -> bool {
    matches!(task_kind, "compose.up" | "compose.down" | "compose.restart")
}

fn is_binary_task_kind(task_kind: &str) -> bool {
    matches!(task_kind, "binary.restart" | "binary.stop")
}

fn is_node_install_task_kind(task_kind: &str) -> bool {
    matches!(
        task_kind,
        "node.install.docker"
            | "node.install.compose"
            | "node.install.caddy"
            | "node.install.nginx"
    )
}

fn task_error_response(err: TaskError) -> Response {
    let status = task_error_status(&err);
    (status, err.message().to_owned()).into_response()
}

fn task_error_status(err: &TaskError) -> StatusCode {
    match err {
        TaskError::NotFound(_) => StatusCode::NOT_FOUND,
        TaskError::InvalidState(_) => StatusCode::BAD_REQUEST,
        TaskError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn task_display_text<'a>(value: &'a str, fallback: &'static str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

fn task_log_stream_tone(stream: &str) -> &'static str {
    match stream {
        "stderr" => "warning",
        "stdout" | "combined" => "active",
        _ => "neutral",
    }
}

fn openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Easy Deploy OpenAPI",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "用于开发者、CI 和 AI 调用 Easy Deploy 部署应用的开放接口。"
        },
        "servers": [
            { "url": "http://127.0.0.1:9066", "description": "本机默认服务" }
        ],
        "security": [{ "BearerAuth": [] }],
        "paths": {
            "/api/v1/nodes": {
                "get": {
                    "summary": "列出节点",
                    "operationId": "listNodes",
                    "responses": {
                        "200": { "description": "节点列表" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" }
                    }
                }
            },
            "/api/v1/apps": {
                "get": {
                    "summary": "列出应用",
                    "operationId": "listApps",
                    "responses": {
                        "200": { "description": "应用列表" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" }
                    }
                },
                "post": {
                    "summary": "创建应用",
                    "operationId": "createApp",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreateAppRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": { "description": "创建成功，返回应用 id" },
                        "400": { "description": "请求体或业务校验失败" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" },
                        "409": { "description": "应用标识冲突" }
                    }
                }
            },
            "/api/v1/apps/{app_id}": {
                "get": {
                    "summary": "应用详情",
                    "operationId": "getApp",
                    "parameters": [
                        { "name": "app_id", "in": "path", "required": true, "schema": { "type": "integer" } }
                    ],
                    "responses": {
                        "200": { "description": "应用详情" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" }
                    }
                }
            },
            "/api/v1/apps/{app_id}/deploy": {
                "post": {
                    "summary": "触发部署任务",
                    "operationId": "deployApp",
                    "parameters": [
                        { "name": "app_id", "in": "path", "required": true, "schema": { "type": "integer" } }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/DeployAppRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": { "description": "已创建部署任务，返回 task_id" },
                        "400": { "description": "不支持的动作或应用状态不满足部署条件" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" }
                    }
                }
            },
            "/api/v1/tasks": {
                "get": {
                    "summary": "列出任务",
                    "operationId": "listTasks",
                    "parameters": [
                        { "name": "status", "in": "query", "schema": { "type": "string" } },
                        { "name": "phase", "in": "query", "schema": { "type": "string" } },
                        { "name": "app_id", "in": "query", "schema": { "type": "integer" } },
                        { "name": "task_kind", "in": "query", "schema": { "type": "string" } },
                        { "name": "q", "in": "query", "schema": { "type": "string" } }
                    ],
                    "responses": {
                        "200": { "description": "任务列表" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" }
                    }
                }
            },
            "/api/v1/tasks/{task_id}": {
                "get": {
                    "summary": "任务详情",
                    "operationId": "getTask",
                    "parameters": [
                        { "name": "task_id", "in": "path", "required": true, "schema": { "type": "integer" } }
                    ],
                    "responses": {
                        "200": { "description": "任务、日志和节点结果" },
                        "401": { "description": "缺少或无效 Token" },
                        "403": { "description": "权限不足" },
                        "404": { "description": "任务不存在" }
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "BearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            },
            "schemas": {
                "CreateAppRequest": {
                    "type": "object",
                    "required": ["app_key", "name"],
                    "properties": {
                        "app_key": { "type": "string", "description": "应用唯一标识，例如 orders-api" },
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "app_type": { "type": "string", "enum": ["compose", "binary"], "default": "compose" },
                        "deploy_strategy": { "type": "string", "enum": ["rolling", "all_at_once"], "default": "rolling" },
                        "work_dir": { "type": "string", "description": "为空时使用平台默认目录模板" },
                        "target_node_ids": { "type": "array", "items": { "type": "integer" } },
                        "compose_content": { "type": "string" },
                        "env_content": { "type": "string" },
                        "binary_artifact_version": { "type": "string" },
                        "binary_artifact_path": { "type": "string" },
                        "binary_exec_args": { "type": "string" },
                        "binary_service_user": { "type": "string" },
                        "binary_unit_name": { "type": "string" },
                        "binary_release_strategy": { "type": "string" },
                        "binary_active_slot": { "type": "string" },
                        "binary_base_port": { "type": "integer" },
                        "binary_standby_port": { "type": "integer" },
                        "binary_proxy_enabled": { "type": "boolean" },
                        "binary_proxy_kind": { "type": "string" },
                        "binary_proxy_domain": { "type": "string" },
                        "binary_proxy_config_path": { "type": "string" }
                    }
                },
                "DeployAppRequest": {
                    "type": "object",
                    "required": ["action"],
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["up", "down", "restart", "binary_restart", "binary_stop"]
                        }
                    }
                }
            }
        }
    })
}

fn openapi_docs_html() -> String {
    r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Easy Deploy OpenAPI</title>
  <style>
    :root { color-scheme: light; --bg:#f6f7f9; --panel:#fff; --text:#172033; --muted:#657084; --line:#dfe4ec; --accent:#176bff; --code:#101828; }
    * { box-sizing: border-box; }
    body { margin:0; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background:var(--bg); color:var(--text); }
    header { padding:32px max(24px, calc((100vw - 1120px) / 2)); background:#111827; color:#fff; }
    header h1 { margin:0 0 8px; font-size:30px; letter-spacing:0; }
    header p { margin:0; color:#cbd5e1; max-width:760px; line-height:1.7; }
    main { width:min(1120px, calc(100vw - 32px)); margin:24px auto 48px; display:grid; gap:18px; }
    section { background:var(--panel); border:1px solid var(--line); border-radius:8px; padding:22px; }
    h2 { margin:0 0 14px; font-size:20px; }
    h3 { margin:18px 0 10px; font-size:15px; }
    p, li { color:var(--muted); line-height:1.7; }
    a { color:var(--accent); text-decoration:none; }
    a:hover { text-decoration:underline; }
    code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
    code { color:var(--code); background:#eef2f7; border-radius:4px; padding:2px 5px; }
    pre { margin:12px 0 0; padding:14px; overflow:auto; border-radius:8px; background:#101828; color:#e5e7eb; line-height:1.55; }
    .grid { display:grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap:14px; }
    .endpoint { border:1px solid var(--line); border-radius:8px; padding:14px; }
    .method { display:inline-flex; min-width:54px; justify-content:center; border-radius:4px; padding:3px 8px; background:#dbeafe; color:#1d4ed8; font-weight:700; font-size:12px; }
    .path { margin-left:8px; font-family:ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-weight:700; }
  </style>
</head>
<body>
  <header>
    <h1>Easy Deploy OpenAPI</h1>
    <p>给开发者、CI 脚本和 AI 调用的部署接口。文档无需登录即可访问，实际 API 需要在后台生成 API Token 后通过 Bearer Token 调用。</p>
  </header>
  <main>
    <section>
      <h2>认证</h2>
      <p>进入后台 <code>/admin/api-tokens</code> 生成 Token。每个调用方单独生成，并填写来源，例如 <code>ci-github-actions</code>、<code>ai-agent-prod</code>。服务端只保存 Token 哈希，明文只显示一次。</p>
      <pre><code>Authorization: Bearer &lt;your_api_token&gt;</code></pre>
      <p>机器可读取原始 OpenAPI JSON：<a href="/openapi.json">/openapi.json</a></p>
    </section>

    <section>
      <h2>推荐流程</h2>
      <ol>
        <li>调用 <code>GET /api/v1/nodes</code> 获取可部署节点。</li>
        <li>调用 <code>POST /api/v1/apps</code> 创建应用并绑定节点。</li>
        <li>调用 <code>POST /api/v1/apps/{app_id}/deploy</code> 触发部署。</li>
        <li>调用 <code>GET /api/v1/tasks/{task_id}</code> 查询任务、日志和节点结果。</li>
      </ol>
    </section>

    <section>
      <h2>接口</h2>
      <div class="grid">
        <div class="endpoint"><span class="method">GET</span><span class="path">/api/v1/nodes</span><p>列出节点、状态和基础能力。</p></div>
        <div class="endpoint"><span class="method">GET</span><span class="path">/api/v1/apps</span><p>列出应用。</p></div>
        <div class="endpoint"><span class="method">POST</span><span class="path">/api/v1/apps</span><p>创建 compose 或 binary 应用。</p></div>
        <div class="endpoint"><span class="method">GET</span><span class="path">/api/v1/apps/{id}</span><p>查看应用配置、目标节点和运行状态。</p></div>
        <div class="endpoint"><span class="method">POST</span><span class="path">/api/v1/apps/{id}/deploy</span><p>触发 up、down、restart 或二进制服务操作。</p></div>
        <div class="endpoint"><span class="method">GET</span><span class="path">/api/v1/tasks</span><p>列出最近任务，支持状态和应用筛选。</p></div>
        <div class="endpoint"><span class="method">GET</span><span class="path">/api/v1/tasks/{id}</span><p>查看任务详情、日志和节点执行结果。</p></div>
      </div>
    </section>

    <section>
      <h2>curl 示例</h2>
      <h3>列出节点</h3>
      <pre><code>curl -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  http://127.0.0.1:9066/api/v1/nodes</code></pre>
      <h3>创建 Compose 应用</h3>
      <pre><code>curl -X POST http://127.0.0.1:9066/api/v1/apps \
  -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "app_key": "orders-api",
    "name": "Orders API",
    "app_type": "compose",
    "target_node_ids": [1],
    "compose_content": "services:\n  web:\n    image: nginx:alpine\n    ports:\n      - \"8080:80\"\n"
  }'</code></pre>
      <h3>触发部署</h3>
      <pre><code>curl -X POST http://127.0.0.1:9066/api/v1/apps/1/deploy \
  -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"action":"up"}'</code></pre>
    </section>
  </main>
</body>
</html>"#
        .to_owned()
}

async fn record_audit_event(
    state: &AppState,
    session: &CurrentSession,
    action: &str,
    target_type: &str,
    target_id: &str,
    message: &str,
) {
    if let Err(err) = state
        .auth()
        .record_system_audit(session, action, target_type, target_id, message)
        .await
    {
        warn!("failed to record audit event {action}: {err}");
    }
}

fn compose_audit_action(action: ComposeTaskAction) -> &'static str {
    match action {
        ComposeTaskAction::Up => "deploy.compose_up",
        ComposeTaskAction::Down => "deploy.compose_down",
        ComposeTaskAction::Restart => "deploy.compose_restart",
    }
}

fn binary_audit_action(action: BinaryTaskAction) -> &'static str {
    match action {
        BinaryTaskAction::Restart => "deploy.binary_restart",
        BinaryTaskAction::Stop => "deploy.binary_stop",
    }
}

fn compose_result_view(value: crate::deploy::ComposeCommandOutput) -> ComposeResultView {
    let output = if value.output.trim().is_empty() {
        "命令没有输出".to_owned()
    } else {
        value.output
    };
    ComposeResultView {
        command: value.command,
        status: if value.success { "成功" } else { "失败" },
        status_tone: if value.success { "success" } else { "warning" },
        status_code: value
            .status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "无".to_owned()),
        output,
    }
}

fn service_log_href(app_id: i64, service_name: &str, node_id: i64, tail_lines: u16) -> String {
    format!("/services/{app_id}/{service_name}/logs?node_id={node_id}&tail={tail_lines}")
}

fn normalize_log_tail_lines(value: Option<u16>) -> u16 {
    value.unwrap_or(200).clamp(50, 1000)
}

fn service_log_tail_options(
    app_id: i64,
    service_name: &str,
    node_id: i64,
    active_tail_lines: u16,
) -> Vec<ServiceLogTailOptionRow> {
    [50_u16, 200, 500, 1000]
        .into_iter()
        .map(|tail_lines| ServiceLogTailOptionRow {
            label: format!("{tail_lines} 行"),
            href: service_log_href(app_id, service_name, node_id, tail_lines),
            active: tail_lines == active_tail_lines,
        })
        .collect()
}

fn nav_sections<'a>(active_path: &str, session: &CurrentSession) -> Vec<NavSection<'a>> {
    let sections = [
        (
            "工作台",
            vec![
                nav_item("总览", "/", "dashboard", active_path),
                nav_item("部署任务", "/tasks", "tasks", active_path),
            ],
        ),
        (
            "部署",
            vec![
                nav_item("应用", "/apps", "apps", active_path),
                nav_item("模板", "/templates", "templates", active_path),
                nav_item("制品", "/artifacts", "artifacts", active_path),
            ],
        ),
        (
            "资源",
            vec![
                nav_item("节点", "/nodes", "nodes", active_path),
                nav_item("凭据", "/node-credentials", "credentials", active_path),
            ],
        ),
        (
            "权限",
            vec![
                nav_item("账号", "/admin/accounts", "accounts", active_path),
                nav_item("角色", "/admin/roles", "roles", active_path),
                nav_item("权限", "/admin/permissions", "permissions", active_path),
                nav_item("会话", "/admin/sessions", "sessions", active_path),
            ],
        ),
        (
            "系统",
            vec![
                nav_item("审计", "/audit", "audit", active_path),
                nav_item("设置", "/settings", "settings", active_path),
                nav_item("个人", "/profile", "profile", active_path),
            ],
        ),
    ];

    let _extra_sections = [(
        "开放接口",
        vec![nav_item(
            "API Token",
            "/admin/api-tokens",
            "tokens",
            active_path,
        )],
    )];

    let mut sections = sections
        .into_iter()
        .filter_map(|(label, items)| {
            let items = items
                .into_iter()
                .filter(|item| {
                    nav_permission(item.href)
                        .map(|permission| session.can(permission))
                        .unwrap_or(true)
                })
                .collect::<Vec<_>>();
            (!items.is_empty()).then_some(NavSection { label, items })
        })
        .collect::<Vec<_>>();

    if let Some(section) = sections.iter_mut().find(|section| {
        section
            .items
            .iter()
            .any(|item| item.href == "/admin/sessions")
    }) {
        let item = nav_item("API Token", "/admin/api-tokens", "tokens", active_path);
        if nav_permission(item.href)
            .map(|permission| session.can(permission))
            .unwrap_or(true)
        {
            section.items.push(item);
        }
    }

    sections
}

fn nav_item<'a>(label: &'a str, href: &'a str, icon: &'a str, active_path: &str) -> NavItem<'a> {
    NavItem {
        label,
        href,
        icon,
        active: active_path == href,
    }
}

fn parse_id_csv(value: Option<&str>) -> Vec<i64> {
    value
        .unwrap_or_default()
        .split(',')
        .filter_map(|item| item.trim().parse::<i64>().ok())
        .collect()
}

fn valid_csrf(session: &CurrentSession, submitted: &str) -> bool {
    !submitted.is_empty() && submitted == session.csrf_token
}

fn parse_create_app_form(bytes: &[u8]) -> Result<CreateAppForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(CreateAppForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        app_key: required_form_value(&fields, "app_key")?,
        name: required_form_value(&fields, "name")?,
        description: first_form_value(&fields, "description"),
        app_type: required_form_value(&fields, "app_type")?,
        deploy_strategy: first_form_value(&fields, "deploy_strategy"),
        work_dir: first_form_value(&fields, "work_dir"),
        compose_content: first_form_value(&fields, "compose_content"),
        env_content: first_form_value(&fields, "env_content"),
        binary_artifact_version: first_form_value(&fields, "binary_artifact_version"),
        binary_artifact_path: first_form_value(&fields, "binary_artifact_path"),
        binary_exec_args: first_form_value(&fields, "binary_exec_args"),
        binary_service_user: first_form_value(&fields, "binary_service_user"),
        binary_unit_name: first_form_value(&fields, "binary_unit_name"),
        binary_release_strategy: first_form_value(&fields, "binary_release_strategy"),
        binary_active_slot: first_form_value(&fields, "binary_active_slot"),
        binary_base_port: optional_form_i64(&fields, "binary_base_port")?,
        binary_standby_port: optional_form_i64(&fields, "binary_standby_port")?,
        binary_proxy_enabled: form_bool(&fields, "binary_proxy_enabled"),
        binary_proxy_kind: first_form_value(&fields, "binary_proxy_kind"),
        binary_proxy_domain: first_form_value(&fields, "binary_proxy_domain"),
        binary_proxy_config_path: first_form_value(&fields, "binary_proxy_config_path"),
        target_node_ids: parse_form_ids(&fields, "target_node_ids")?,
    })
}

fn parse_create_template_app_form(bytes: &[u8]) -> Result<CreateTemplateAppForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(CreateTemplateAppForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        template_key: required_form_value(&fields, "template_key")?,
        app_key: required_form_value(&fields, "app_key")?,
        name: required_form_value(&fields, "name")?,
        description: first_form_value(&fields, "description"),
        work_dir: first_form_value(&fields, "work_dir"),
        deploy_strategy: first_form_value(&fields, "deploy_strategy"),
        port: required_form_u16(&fields, "port")?,
        target_node_ids: parse_form_ids(&fields, "target_node_ids")?,
    })
}

fn parse_update_app_metadata_form(bytes: &[u8]) -> Result<UpdateAppMetadataForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(UpdateAppMetadataForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        name: required_form_value(&fields, "name")?,
        description: first_form_value(&fields, "description"),
        work_dir: required_form_value(&fields, "work_dir")?,
        deploy_strategy: first_form_value(&fields, "deploy_strategy"),
        target_node_ids: parse_form_ids(&fields, "target_node_ids")?,
    })
}

fn parse_create_account_form(bytes: &[u8]) -> Result<CreateAccountForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(CreateAccountForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        username: required_form_value(&fields, "username")?,
        display_name: first_form_value(&fields, "display_name"),
        password: required_form_value(&fields, "password")?,
        role_ids: parse_form_ids(&fields, "role_ids")?,
    })
}

fn parse_account_roles_form(bytes: &[u8]) -> Result<AccountRolesForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(AccountRolesForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        account_id: required_form_id(&fields, "account_id")?,
        role_ids: parse_form_ids(&fields, "role_ids")?,
    })
}

fn parse_create_role_form(bytes: &[u8]) -> Result<CreateRoleForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(CreateRoleForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        role_code: required_form_value(&fields, "role_code")?,
        role_name: required_form_value(&fields, "role_name")?,
        description: first_form_value(&fields, "description"),
        permission_ids: parse_form_ids(&fields, "permission_ids")?,
    })
}

fn parse_role_permissions_form(bytes: &[u8]) -> Result<RolePermissionsForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(RolePermissionsForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        role_id: required_form_id(&fields, "role_id")?,
        permission_ids: parse_form_ids(&fields, "permission_ids")?,
    })
}

fn parse_urlencoded_fields(bytes: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(bytes).into_owned().collect()
}

fn first_form_value(fields: &[(String, String)], name: &str) -> String {
    fields
        .iter()
        .find_map(|(key, value)| (key == name).then(|| value.clone()))
        .unwrap_or_default()
}

fn form_bool(fields: &[(String, String)], name: &str) -> bool {
    fields
        .iter()
        .any(|(key, value)| key == name && matches!(value.as_str(), "true" | "1" | "on" | "yes"))
}

fn required_form_value(fields: &[(String, String)], name: &str) -> Result<String, String> {
    let value = first_form_value(fields, name);
    if value.trim().is_empty() {
        Err(format!("缺少表单字段 {name}"))
    } else {
        Ok(value)
    }
}

fn required_form_id(fields: &[(String, String)], name: &str) -> Result<i64, String> {
    required_form_value(fields, name)?
        .parse::<i64>()
        .map_err(|_| format!("表单字段 {name} 必须是数字"))
}

fn required_form_u16(fields: &[(String, String)], name: &str) -> Result<u16, String> {
    required_form_value(fields, name)?
        .parse::<u16>()
        .map_err(|_| format!("表单字段 {name} 必须是数字"))
}

fn optional_form_i64(fields: &[(String, String)], name: &str) -> Result<i64, String> {
    let value = first_form_value(fields, name);
    if value.trim().is_empty() {
        return Ok(0);
    }
    value
        .parse::<i64>()
        .map_err(|_| format!("表单字段 {name} 必须是数字"))
}

fn parse_form_ids(fields: &[(String, String)], name: &str) -> Result<Vec<i64>, String> {
    fields
        .iter()
        .filter(|(key, value)| key == name && !value.trim().is_empty())
        .map(|(_, value)| {
            value
                .parse::<i64>()
                .map_err(|_| format!("表单字段 {name} 必须是数字"))
        })
        .collect()
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "无权限访问该页面").into_response()
}

fn login_notice_message(notice: Option<&str>) -> Option<&'static str> {
    match notice {
        Some("required") => Some("请先登录后再访问部署控制台。"),
        Some("expired") => Some("登录状态已失效，请重新登录。"),
        Some("logout") => Some("已退出登录。"),
        _ => None,
    }
}

fn account_notice_message(notice: Option<&str>) -> Option<&'static str> {
    match notice {
        Some("created") => Some("账号已创建，初始角色已生效。"),
        Some("status") => Some("账号状态已更新，受影响会话已同步处理。"),
        Some("password") => Some("账号密码已重置，旧会话已强制失效。"),
        Some("roles") => Some("账号角色已更新，旧会话已强制失效。"),
        _ => None,
    }
}

fn session_notice_message(notice: Option<&str>) -> Option<&'static str> {
    match notice {
        Some("revoked") => Some("会话已强制下线。"),
        _ => None,
    }
}

fn api_token_notice_message(notice: Option<&str>) -> Option<&'static str> {
    match notice {
        Some("revoked") => Some("API Token 已吊销。"),
        _ => None,
    }
}

fn app_detail_notice_message(notice: Option<&str>) -> Option<&'static str> {
    match notice {
        Some("created") => Some("应用已创建，按下面的下一步完成首次部署。"),
        _ => None,
    }
}

fn node_check_return_path(return_to: Option<&str>) -> &str {
    let Some(path) = return_to.map(str::trim).filter(|path| !path.is_empty()) else {
        return "/nodes";
    };
    if (path.starts_with("/apps/")
        || path == "/nodes"
        || path.starts_with("/nodes/")
        || path == "/services"
        || path.starts_with("/services/")
        || path.starts_with("/tasks/"))
        && !path.starts_with("//")
        && !path.contains('\r')
        && !path.contains('\n')
    {
        path
    } else {
        "/nodes"
    }
}

fn task_return_path(return_to: Option<&str>) -> Option<&str> {
    let path = return_to?.trim();
    if (path.starts_with("/apps/")
        || path == "/nodes"
        || path.starts_with("/nodes/")
        || path == "/services"
        || path.starts_with("/services/")
        || path.starts_with("/tasks/"))
        && !path.starts_with("//")
        && !path.contains('\r')
        && !path.contains('\n')
    {
        Some(path)
    } else {
        None
    }
}

fn task_detail_redirect_path(task_id: i64, return_to: Option<&str>) -> String {
    let path = format!("/tasks/{task_id}");
    let Some(return_to) = task_return_path(return_to) else {
        return path;
    };
    format!("{path}?return_to={}", encode_query_value(return_to))
}

fn task_return_action_view(return_to: Option<&str>) -> TaskReturnActionView {
    let Some(path) = return_to else {
        return TaskReturnActionView {
            path: String::new(),
            back_label: "",
            check_label: "",
            hint: "",
            has_return: false,
        };
    };
    let (back_label, check_label, hint) = if path.contains("/confirm") {
        (
            "返回部署确认",
            "重新探测并返回确认页",
            "安装完成后先刷新节点能力，再回到部署确认页提交任务。",
        )
    } else if path.starts_with("/tasks/") {
        (
            "返回来源任务",
            "重新探测并返回任务",
            "安装完成后先刷新节点能力，再回到来源任务查看修复结果。",
        )
    } else if path.starts_with("/nodes/") {
        (
            "返回节点详情",
            "重新探测并返回节点详情",
            "安装完成后刷新节点能力，并回到节点详情确认组件状态。",
        )
    } else if path.starts_with("/services/") {
        (
            "返回运行项日志",
            "重新探测并返回运行项日志",
            "处理完成后回到运行项日志，继续查看该节点的运行上下文。",
        )
    } else if path == "/services" {
        (
            "返回运行项列表",
            "重新探测并返回运行项列表",
            "处理完成后回到运行项列表，继续查看运行项和节点状态。",
        )
    } else {
        (
            "返回上一页",
            "重新探测并返回",
            "安装完成后刷新节点能力，再回到来源页面继续操作。",
        )
    };
    TaskReturnActionView {
        path: path.to_owned(),
        back_label,
        check_label,
        hint,
        has_return: true,
    }
}

fn encode_query_value(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('?', "%3F")
        .replace('&', "%26")
        .replace('=', "%3D")
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn redirect_with_auth_cookies(path: &str, tokens: &SessionTokens, secure: bool) -> Response {
    let mut response = redirect(path);
    append_cookie(
        &mut response,
        &auth_cookie("ed_access", &tokens.access_token, secure),
    );
    append_cookie(
        &mut response,
        &auth_cookie("ed_refresh", &tokens.refresh_token, secure),
    );
    response
}

fn redirect_with_expired_auth_cookies(path: &str, secure: bool) -> Response {
    let mut response = redirect(path);
    append_cookie(&mut response, &expired_auth_cookie("ed_access", secure));
    append_cookie(&mut response, &expired_auth_cookie("ed_refresh", secure));
    response
}

fn redirect(path: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, path.to_owned())],
        "",
    )
        .into_response()
}

fn auth_cookie(name: &str, value: &str, secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax{secure_attribute}")
}

fn expired_auth_cookie(name: &str, secure: bool) -> String {
    let secure_attribute = if secure { "; Secure" } else { "" };
    format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure_attribute}")
}

fn append_cookie(response: &mut Response, cookie: &str) {
    response.headers_mut().append(
        header::SET_COOKIE,
        cookie.parse().expect("valid Set-Cookie header"),
    );
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookie| {
            cookie.split(';').find_map(|part| {
                let mut pieces = part.trim().splitn(2, '=');
                let key = pieces.next()?.trim();
                let value = pieces.next()?.trim();
                (key == name).then(|| value.to_owned())
            })
        })
}

fn is_node_check_ajax_request(headers: &HeaderMap) -> bool {
    headers
        .get("x-requested-with")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "easy-deploy-node-check")
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn request_client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_default()
        .to_owned()
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(ApiErrorBody { error: message })).into_response()
}

impl FromRequestParts<AppState> for CurrentSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(access_token) = cookie_value(&parts.headers, "ed_access") else {
            return Err(redirect("/login?notice=required"));
        };
        state
            .auth()
            .authenticate_access_token(&access_token)
            .await
            .map_err(|_| {
                redirect_with_expired_auth_cookies(
                    "/login?notice=expired",
                    state.settings().cookie_secure,
                )
            })
    }
}

#[derive(Clone, Debug)]
struct ApiSession {
    inner: crate::auth::ApiTokenAuthSession,
}

impl ApiSession {
    fn can(&self, permission_key: &str) -> bool {
        self.inner.session.can(permission_key)
    }

    fn actor(&self) -> String {
        format!(
            "api:{}@{}",
            self.inner.source, self.inner.session.account.username
        )
    }
}

impl FromRequestParts<AppState> for ApiSession {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers)
            .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        let client_ip = request_client_ip(&parts.headers);
        state
            .auth()
            .authenticate_api_token(&token, &client_ip)
            .await
            .map(|inner| Self { inner })
            .map_err(|err| api_error(err.status_code(), err.message()))
    }
}

pub struct HtmlTemplateError(askama::Error);

impl IntoResponse for HtmlTemplateError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("render template failed: {}", self.0),
        )
            .into_response()
    }
}

impl From<askama::Error> for HtmlTemplateError {
    fn from(value: askama::Error) -> Self {
        Self(value)
    }
}

pub fn html_response(html: String) -> Response {
    Html(html).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::sqlite::SqliteConnectOptions;
    use tower::ServiceExt;

    use crate::{
        apps::AppService,
        auth::{AuthService, MemorySessionStore},
        deploy::{ComposeExecutor, SystemdExecutor, TokioCommandRunner},
        nodes::NodeService,
        runtimefs::RuntimeFs,
        tasks::TaskService,
    };

    use super::*;

    async fn test_app_with_auth() -> (Router, AuthService) {
        let db = sqlx::SqlitePool::connect_with(
            "sqlite::memory:"
                .parse::<SqliteConnectOptions>()
                .expect("valid in-memory sqlite url")
                .foreign_keys(true),
        )
        .await
        .expect("connect in-memory sqlite");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("run migrations");
        let auth = AuthService::new(db.clone(), Arc::new(MemorySessionStore::new()));
        auth.sync_permission_registry()
            .await
            .expect("sync permission registry");
        let auth_for_test = auth.clone();
        let settings = Settings {
            bind: "127.0.0.1:0".parse().expect("valid bind address"),
            database_url: "sqlite::memory:".to_owned(),
            data_dir: ".".into(),
            cookie_secure: false,
            uploaded_binary_releases_to_keep: 4,
            command_timeout_secs: 120,
        };

        let tasks = TaskService::new(db.clone());
        let platform = PlatformConfigService::new(db.clone());
        let command_runner = Arc::new(TokioCommandRunner::new(settings.command_timeout_secs));
        let nodes = NodeService::new(db.clone(), command_runner.clone());
        let node_credentials = NodeCredentialService::new(db.clone(), ".");
        let apps = AppService::new(
            db.clone(),
            RuntimeFs::new("."),
            ComposeExecutor::new(command_runner.clone()),
            SystemdExecutor::new(command_runner),
            tasks.clone(),
            platform.clone(),
        );

        (
            build_router(AppState::new(
                settings,
                db,
                AppStateServices {
                    auth,
                    nodes,
                    node_credentials,
                    apps,
                    tasks,
                    platform,
                },
            )),
            auth_for_test,
        )
    }

    async fn test_app() -> Router {
        test_app_with_auth().await.0
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn dashboard_redirects_to_login_without_session() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&"/login?notice=required".parse().expect("valid header"))
        );
    }

    #[tokio::test]
    async fn login_page_renders_session_notice() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/login?notice=expired")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert!(String::from_utf8_lossy(&body).contains("登录状态已失效，请重新登录。"));
    }

    #[tokio::test]
    async fn openapi_docs_are_public() {
        let app = test_app().await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let spec: serde_json::Value = serde_json::from_slice(&body).expect("openapi json response");
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(
            spec["components"]["securitySchemes"]["BearerAuth"]["scheme"],
            "bearer"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/docs/openapi")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert!(String::from_utf8_lossy(&body).contains("Easy Deploy OpenAPI"));
    }

    #[tokio::test]
    async fn api_v1_requires_bearer_token() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/v1/apps")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert!(String::from_utf8_lossy(&body).contains("missing bearer token"));
    }

    #[tokio::test]
    async fn api_token_can_call_v1_apps() {
        let (app, auth) = test_app_with_auth().await;
        let login = auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let token = auth
            .create_api_token(&login.session, "test-suite")
            .await
            .expect("create api token");

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/apps")
                    .header(header::AUTHORIZATION, format!("Bearer {}", token.token))
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("api apps json response");
        assert!(payload["data"].is_array());

        let listed = auth.list_api_tokens().await.expect("list api tokens");
        assert_eq!(listed[0].source, "test-suite");
        assert!(listed[0].last_used_at.is_some());
    }

    #[tokio::test]
    async fn favicon_returns_svg() {
        for uri in ["/favicon.svg", "/favicon.ico"] {
            let response = test_app()
                .await
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build request"),
                )
                .await
                .expect("send request");

            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(
                    &"image/svg+xml; charset=utf-8"
                        .parse()
                        .expect("valid header")
                )
            );

            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read body");
            assert!(String::from_utf8_lossy(&body).contains("Easy Deploy"));
        }
    }

    #[test]
    fn auth_cookie_adds_secure_only_when_enabled() {
        let plain = auth_cookie("ed_access", "access-token", false);
        assert_eq!(
            plain,
            "ed_access=access-token; Path=/; HttpOnly; SameSite=Lax"
        );

        let secure = auth_cookie("ed_refresh", "refresh-token", true);
        assert_eq!(
            secure,
            "ed_refresh=refresh-token; Path=/; HttpOnly; SameSite=Lax; Secure"
        );

        let expired_secure = expired_auth_cookie("ed_access", true);
        assert_eq!(
            expired_secure,
            "ed_access=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );
    }

    #[test]
    fn node_capability_guides_show_missing_components_and_ready_state() {
        let mut node = crate::nodes::NodeListItem {
            id: 2,
            node_key: "prod-a".to_owned(),
            name: "生产节点 A".to_owned(),
            node_type: "ssh".to_owned(),
            address: "10.0.2.11".to_owned(),
            ssh_port: 22,
            ssh_user: "deploy".to_owned(),
            credential_id: Some(7),
            credential_name: Some("生产部署密钥".to_owned()),
            credential_fingerprint: Some("SHA256:test".to_owned()),
            credential_private_key_path: Some("/tmp/easy-deploy/id_ed25519".to_owned()),
            work_dir: "/opt/easy-deploy/apps".to_owned(),
            region: "prod".to_owned(),
            labels: "prod".to_owned(),
            status: "offline".to_owned(),
            docker_status: "unknown".to_owned(),
            last_check_at: None,
            last_message: Some("SSH Docker daemon 不可用: Cannot connect".to_owned()),
            capability_status: "failed".to_owned(),
            docker_available: 0,
            compose_available: 0,
            systemd_available: 0,
            caddy_available: 0,
            nginx_available: 0,
            last_docker_version: None,
            last_compose_version: None,
            last_os_info: None,
            last_disk_info: None,
            last_systemd_version: Some("systemd 探测失败: permission denied".to_owned()),
            last_caddy_version: None,
            last_nginx_version: None,
        };

        let missing = node_capability_guides(&node);
        assert_eq!(missing.len(), 4);
        assert_eq!(missing[0].title, "安装 Docker Engine");
        assert!(
            missing[0]
                .command
                .contains("ssh -p 22 -i /tmp/easy-deploy/id_ed25519 -o IdentitiesOnly=yes deploy@10.0.2.11 curl -fsSL https://get.docker.com")
        );
        assert_eq!(missing[1].title, "确认 systemd 可用");
        assert!(
            missing[1]
                .command
                .contains("ssh -p 22 -i /tmp/easy-deploy/id_ed25519 -o IdentitiesOnly=yes deploy@10.0.2.11 systemctl --version")
        );
        assert_eq!(missing[2].title, "安装 Caddy");
        assert!(missing[2].command.contains("caddy"));
        assert_eq!(missing[3].title, "安装 Nginx");
        assert!(missing[3].command.contains("nginx"));

        node.docker_available = 1;
        node.last_docker_version = Some("Docker version 27.0.2".to_owned());
        node.last_message = Some("Docker Compose 不可用: plugin missing".to_owned());
        let compose_missing = node_capability_guides(&node);
        assert_eq!(compose_missing[0].title, "安装 Docker Compose 插件");
        assert!(compose_missing[0].command.contains("docker-compose-plugin"));

        node.compose_available = 1;
        node.systemd_available = 1;
        node.caddy_available = 1;
        node.nginx_available = 1;
        node.last_compose_version = Some("Docker Compose version v2.29.0".to_owned());
        node.last_systemd_version = Some("systemd 254".to_owned());
        node.last_caddy_version = Some("2.8.4".to_owned());
        node.last_nginx_version = Some("nginx version: nginx/1.24.0".to_owned());
        let ready = node_capability_guides(&node);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].title, "节点能力已就绪");
        assert_eq!(
            ready[0].command,
            "ssh -p 22 -i /tmp/easy-deploy/id_ed25519 -o IdentitiesOnly=yes deploy@10.0.2.11 docker compose version"
        );
    }
}
#[derive(Deserialize)]
struct CsrfForm {
    csrf_token: String,
}

#[derive(Deserialize)]
struct ConfirmTaskForm {
    csrf_token: String,
    confirmed: Option<String>,
}

impl ConfirmTaskForm {
    fn is_confirmed(&self) -> bool {
        self.confirmed.as_deref() == Some("1")
    }
}
