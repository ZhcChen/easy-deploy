mod templates;

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Form, Json, Router,
    extract::{FromRequestParts, Multipart, Path, Query, RawForm, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::warn;

use crate::{
    Settings,
    application_config::ApplicationConfigService,
    application_releases::{
        ApplicationReleaseError, ApplicationReleaseService, CreateApplicationReleaseInput,
        EnvironmentConfigSelection, RegisterUnitReleaseInput, UnitReleaseChange,
        UnitReleaseStorage, validate_version,
    },
    apps::{
        APP_DEPLOYMENT_IN_PROGRESS_MESSAGE, AppDeployDiffStatus, AppError, AppService,
        BinaryPackageNameError, CompleteReleasePackageUploadInput, ComposeTaskAction,
        CreateAppInput, CreateReleasePackageUploadInput, RELEASE_PACKAGE_EXAMPLE,
        RELEASE_PACKAGE_PATTERN, ServiceTargetNodeItem, UpdateAppConfigInput,
        UpdateAppMetadataInput, UploadReleasePackageInput, artifact_metadata_value,
        normalize_deploy_strategy, normalize_release_source,
        parse_release_package_name_for_service, release_publish_mode_label,
    },
    auth::{
        API_TOKENS_MANAGE, API_TOKENS_VIEW, APPS_STATUS, APPS_VIEW, ARTIFACTS_UPLOAD,
        ARTIFACTS_VIEW, AUDIT_VIEW, AuditLogFilter, AuthService, CurrentSession, DASHBOARD_VIEW,
        LoginInput, NODES_INSTALL, NODES_MANAGE, NODES_VIEW, PROFILE_VIEW, RBAC_ACCOUNTS_VIEW,
        RBAC_PERMISSIONS_VIEW, RBAC_ROLES_VIEW, RBAC_SESSIONS_VIEW, SERVICES_DEPLOY,
        SERVICES_DEPLOY_CANCEL, SERVICES_DEPLOY_RECONCILE, SERVICES_LOGS, SERVICES_VIEW,
        SETTINGS_UPDATE, SETTINGS_VIEW, SessionTokens, TASKS_RETRY, TASKS_VIEW, TEMPLATES_VIEW,
        nav_permission, permission_dependencies,
    },
    catalog::compose_templates,
    deployment_console::DeploymentConsoleService,
    deployment_orchestrator::{
        CreateDeploymentRunInput, DeploymentCancellationRegistry, DeploymentUnitExecutor,
    },
    deployment_orchestrator::{DeploymentAction, DeploymentMode, DeploymentOrchestratorService},
    deployment_retention::{DeploymentLogService, DeploymentRetentionService},
    events::{EventLogError, EventLogFilter, EventLogService},
    health::{HealthCheckKind, normalize_health_config},
    host_metrics::HostMetricsService,
    node_credentials::{
        CreateGeneratedCredentialInput, CreateUploadedCredentialInput, NodeCredentialError,
        NodeCredentialService,
    },
    nodes::{CreateNodeInput, NodeError, NodeInstallComponent, NodeService, UpdateNodeInput},
    platform::{PlatformConfigError, PlatformConfigService, UpdatePlatformConfigInput},
    runtimefs::DeployScriptSet,
    runtimefs::RuntimeFs,
    tasks::{TaskError, TaskListFilter, TaskService},
};
use templates::{
    AccountRow, AccountsTemplate, ApiTokenPageRow, ApiTokensTemplate, AppConfigSnapshotRow,
    AppDeployDiffRow, AppDeployDiffView, AppDeploymentRunRow, AppDetailTemplate, AppNodeChoiceRow,
    AppPageRow, AppRow, AppRuntimeStateRow, AppTargetChoiceRow, ApplicationDeployPlanRow,
    ApplicationDeployTemplate, ApplicationReleaseRow, AppsTemplate, ArtifactAppOptionRow,
    ArtifactPageRow, ArtifactsTemplate, AuditFilterOptionRow, AuditLogRow, AuditTemplate,
    ComposeResultView, DashboardTemplate, DeployConfirmTargetNodeRow, DeployConfirmTemplate,
    DeployPlanFileRow, DeployPlanStepRow, DeployPreflightActionRow, DeployPreflightCheckRow,
    DeployPreflightRow, DeploymentEnvironmentRow, DeploymentTaskControlView, DeploymentUnitRow,
    EnvironmentDeploymentRunRow, EventLogRow, EventsTemplate, LoginTemplate, NavItem, NavSection,
    NodeAppRuntimeRow, NodeCapabilityGuideRow, NodeCheckHistoryRow, NodeCredentialOptionRow,
    NodeCredentialPageRow, NodeCredentialsTemplate, NodeDetailModalRow, NodeDetailTemplate,
    NodePageRow, NodeRow, NodeTaskRow, NodesTemplate, PermissionGroup, PermissionRow,
    PermissionsTemplate, ProfileTemplate, RbacFilterOptionRow, ReleaseQueueRow, RoleRow,
    RolesTemplate, ServiceLogTailOptionRow, ServiceLogsTemplate, ServiceNodeLinkRow,
    ServicePageRow, ServicesTemplate, SessionRow, SessionsTemplate, SettingsRow, SettingsTemplate,
    SummaryItem, TaskAppFilterRow, TaskDetailTemplate, TaskDetailView, TaskExecutionGuideView,
    TaskFilterOptionRow, TaskLogRow, TaskNodeResultRow, TaskPageRow, TaskPhaseGroupRow,
    TaskPhaseStepRow, TaskReturnActionView, TaskRow, TaskStepRow, TasksTemplate, TemplateCardRow,
    TemplatesTemplate, render_html,
};

const LOGO_SVG: &str = include_str!("../../assets/logo.svg");
const APP_JS: &str = include_str!("../../assets/app.js");
const ASSET_VERSION: &str = "20260712-deployment-controls";

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
    pub events: EventLogService,
    pub application_config: Option<ApplicationConfigService>,
    pub application_releases: ApplicationReleaseService,
    pub deployment_orchestrator: DeploymentOrchestratorService,
    pub deployment_console: DeploymentConsoleService,
    pub deployment_executor: Option<Arc<dyn DeploymentUnitExecutor>>,
    pub deployment_logs: DeploymentLogService,
    pub deployment_retention: DeploymentRetentionService,
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
    events: EventLogService,
    application_config: Option<ApplicationConfigService>,
    application_releases: ApplicationReleaseService,
    deployment_orchestrator: DeploymentOrchestratorService,
    deployment_console: DeploymentConsoleService,
    deployment_executor: Option<Arc<dyn DeploymentUnitExecutor>>,
    deployment_logs: DeploymentLogService,
    deployment_retention: DeploymentRetentionService,
    deployment_cancellations: DeploymentCancellationRegistry,
    host_metrics: HostMetricsService,
    api_token_flashes: Mutex<HashMap<String, crate::auth::CreatedApiToken>>,
}

impl AppState {
    pub fn new(settings: Settings, db: SqlitePool, services: AppStateServices) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                host_metrics: HostMetricsService::new(&settings.data_dir),
                settings,
                db,
                auth: services.auth,
                nodes: services.nodes,
                node_credentials: services.node_credentials,
                apps: services.apps,
                tasks: services.tasks,
                platform: services.platform,
                events: services.events,
                application_config: services.application_config,
                application_releases: services.application_releases,
                deployment_orchestrator: services.deployment_orchestrator,
                deployment_console: services.deployment_console,
                deployment_executor: services.deployment_executor,
                deployment_logs: services.deployment_logs,
                deployment_retention: services.deployment_retention,
                deployment_cancellations: DeploymentCancellationRegistry::default(),
                api_token_flashes: Mutex::new(HashMap::new()),
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

    pub fn events(&self) -> &EventLogService {
        &self.inner.events
    }

    pub fn application_config(&self) -> Option<&ApplicationConfigService> {
        self.inner.application_config.as_ref()
    }

    pub fn application_releases(&self) -> &ApplicationReleaseService {
        &self.inner.application_releases
    }

    pub fn deployment_orchestrator(&self) -> &DeploymentOrchestratorService {
        &self.inner.deployment_orchestrator
    }

    pub fn deployment_console(&self) -> &DeploymentConsoleService {
        &self.inner.deployment_console
    }

    pub fn deployment_executor(&self) -> Option<&Arc<dyn DeploymentUnitExecutor>> {
        self.inner.deployment_executor.as_ref()
    }

    pub fn deployment_logs(&self) -> &DeploymentLogService {
        &self.inner.deployment_logs
    }

    pub fn deployment_retention(&self) -> &DeploymentRetentionService {
        &self.inner.deployment_retention
    }

    pub fn deployment_cancellations(&self) -> &DeploymentCancellationRegistry {
        &self.inner.deployment_cancellations
    }

    pub fn host_metrics(&self) -> &HostMetricsService {
        &self.inner.host_metrics
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/api/dashboard/host-metrics", get(dashboard_host_metrics))
        .route("/login", get(login_page).post(login_submit))
        .route("/auth/refresh", post(refresh_submit))
        .route("/logout", post(logout_submit))
        .route("/apps/new", get(new_app_redirect))
        .route("/apps", get(apps_page))
        .route("/apps", post(create_app_submit))
        .route("/apps/{app_id}", get(app_detail_page))
        .route(
            "/apps/{app_id}/deploy",
            get(application_deploy_page).post(application_deploy_submit),
        )
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
        .route(
            "/deployments/{deployment_run_id}/cancel",
            post(deployment_cancel_submit),
        )
        .route(
            "/deployments/{deployment_run_id}/confirm-stopped",
            post(deployment_confirm_stopped_submit),
        )
        .route("/templates", get(templates_page))
        .route("/artifacts", get(artifacts_page))
        .route("/artifacts/upload", post(artifact_upload_submit))
        .route("/artifacts/publish", post(artifact_publish_now_submit))
        .route("/artifacts/schedule", post(artifact_schedule_submit))
        .route(
            "/artifacts/schedule/cancel",
            post(artifact_schedule_cancel_submit),
        )
        .route(
            "/artifacts/queue/cancel",
            post(artifact_queue_cancel_submit),
        )
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
        .route("/admin/api-tokens/delete", post(api_token_delete_submit))
        .route("/profile", get(profile_page))
        .route("/profile/password", post(profile_password_submit))
        .route("/settings", get(settings_page).post(settings_submit))
        .route("/audit", get(audit_page))
        .route("/events", get(events_page))
        .route("/openapi.json", get(openapi_json))
        .route("/docs/openapi", get(openapi_docs))
        .route(
            "/api/v1/services/{service_key}/packages",
            post(api_v1_upload_service_package),
        )
        .route(
            "/api/v1/services/{service_key}/packages/uploads",
            post(api_v1_create_service_package_upload),
        )
        .route(
            "/api/v1/services/{service_key}/packages/uploads/{upload_id}/complete",
            post(api_v1_complete_service_package_upload),
        )
        .route(
            "/api/v1/apps/{app_key}/units/{unit_key}/releases",
            post(api_v1_upload_unit_release),
        )
        .route(
            "/api/v1/apps/{app_key}/releases",
            post(api_v1_create_application_release),
        )
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
                "{} 个健康，{} 个异常，{} 个已停用",
                count_apps_by_runtime(&apps, "healthy"),
                count_apps_by_runtime(&apps, "unhealthy"),
                count_disabled_apps(&apps)
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
            status: app_runtime_status_label(&app.runtime_status),
            status_tone: app_runtime_status_tone(&app.runtime_status),
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

async fn dashboard_host_metrics(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Response {
    if !session.can(DASHBOARD_VIEW) {
        return forbidden();
    }

    Json(state.host_metrics().snapshot().await).into_response()
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
    #[serde(default)]
    environment: String,
    #[serde(default)]
    deploy_strategy: String,
    #[serde(default)]
    release_source: String,
    #[serde(default)]
    auto_queue_release: bool,
    work_dir: String,
    compose_content: String,
    env_content: String,
    #[serde(default)]
    deploy_script_pre_deploy: String,
    #[serde(default)]
    deploy_script_deploy: String,
    #[serde(default)]
    deploy_script_post_deploy: String,
    #[serde(default)]
    deploy_script_switch_traffic: String,
    #[serde(default)]
    deploy_script_cleanup: String,
    #[serde(default)]
    health_check_kind: String,
    #[serde(default)]
    health_endpoint: String,
    #[serde(default)]
    health_timeout_secs: i64,
    #[serde(default)]
    health_expected_status: i64,
    target_node_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct UpdateAppConfigForm {
    csrf_token: String,
    compose_content: String,
    env_content: String,
    #[serde(default)]
    deploy_script_pre_deploy: String,
    #[serde(default)]
    deploy_script_deploy: String,
    #[serde(default)]
    deploy_script_post_deploy: String,
    #[serde(default)]
    deploy_script_switch_traffic: String,
    #[serde(default)]
    deploy_script_cleanup: String,
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
    #[serde(default)]
    environment: String,
    work_dir: String,
    #[serde(default)]
    deploy_strategy: String,
    #[serde(default)]
    release_source: String,
    #[serde(default)]
    auto_queue_release: bool,
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
struct ApiTokenDeleteForm {
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
    artifact_storage_provider: String,
    aliyun_oss_region: String,
    aliyun_oss_endpoint: String,
    aliyun_oss_bucket: String,
    aliyun_oss_object_prefix: String,
    aliyun_oss_access_key_id: String,
    aliyun_oss_access_key_secret: String,
    aliyun_oss_upload_url_ttl_seconds: i64,
    aliyun_oss_download_url_ttl_seconds: i64,
}

#[derive(Deserialize)]
struct ReleasePublishNowForm {
    csrf_token: String,
    release_id: i64,
}

#[derive(Deserialize)]
struct ReleaseScheduleForm {
    csrf_token: String,
    release_id: i64,
    scheduled_publish_at: String,
}

#[derive(Deserialize)]
struct ReleaseCancelScheduleForm {
    csrf_token: String,
    release_id: i64,
}

#[derive(Deserialize)]
struct ReleaseQueueCancelForm {
    csrf_token: String,
    queue_id: i64,
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

#[derive(Deserialize)]
struct DeploymentReconciliationForm {
    csrf_token: String,
    note: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AuditLogQuery {
    action: Option<String>,
    target_type: Option<String>,
    actor: Option<String>,
    q: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct EventLogQuery {
    event_type: Option<String>,
    level: Option<String>,
    target_type: Option<String>,
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
    created: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AppsQuery {
    environment: Option<String>,
    status: Option<String>,
    q: Option<String>,
    page: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AppDetailQuery {
    notice: Option<String>,
    environment_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApplicationDeployQuery {
    environment_id: i64,
    app_release_id: Option<i64>,
    mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ApplicationDeployForm {
    csrf_token: String,
    environment_id: i64,
    app_release_id: i64,
    mode: String,
    expected_plan_hash: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct NodesQuery {
    r#type: Option<String>,
    status: Option<String>,
    q: Option<String>,
}

#[derive(Serialize)]
struct ApiErrorBody<'a> {
    error: &'a str,
}

#[derive(Serialize)]
struct ApiStableErrorBody<'a> {
    code: &'a str,
    error: &'a str,
}

#[derive(Serialize)]
struct ApiPackageErrorBody<'a> {
    code: &'a str,
    error: String,
    expected_pattern: &'a str,
    example: &'a str,
}

struct ApiPackageUploadInput {
    artifact_version: String,
    version_code: Option<i64>,
    published_at: String,
    file_name: String,
    bytes: Vec<u8>,
    entry_file: String,
    source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiCreateApplicationReleaseRequest {
    version: String,
    #[serde(default, alias = "baseAppReleaseId")]
    base_app_release_id: Option<i64>,
    #[serde(default, alias = "unitChanges")]
    unit_changes: Vec<ApiUnitReleaseChange>,
    #[serde(default, alias = "environmentConfigs")]
    environment_configs: Vec<ApiEnvironmentConfigSelection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiUnitReleaseChange {
    #[serde(alias = "unitId")]
    unit_id: i64,
    #[serde(default, alias = "unitReleaseId")]
    unit_release_id: Option<i64>,
    #[serde(default = "default_api_desired_status", alias = "desiredStatus")]
    desired_status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApiEnvironmentConfigSelection {
    #[serde(alias = "environmentId")]
    environment_id: i64,
    #[serde(alias = "configRevisionId")]
    config_revision_id: i64,
}

fn default_api_desired_status() -> String {
    "active".to_owned()
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApiCreatePackageUploadRequest {
    #[serde(default, alias = "fileName")]
    file_name: String,
    #[serde(default, alias = "releaseVersion", alias = "artifact_version")]
    release_version: String,
    #[serde(default, alias = "versionCode")]
    version_code: Option<i64>,
    #[serde(default, alias = "publishedAt")]
    published_at: String,
    #[serde(default)]
    source: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ApiCompletePackageUploadRequest {
    #[serde(default, alias = "checksumSha256")]
    checksum_sha256: String,
    #[serde(default, alias = "sizeBytes")]
    size_bytes: i64,
    #[serde(default, alias = "publishedAt")]
    published_at: String,
    #[serde(default)]
    source: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ArtifactsQuery {
    status: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    q: Option<String>,
    notice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ServiceLogsQuery {
    node_id: Option<i64>,
    tail: Option<u16>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ServicesQuery {
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
    let selected_type = "compose";
    let selected_environment = normalize_app_environment_filter(query.environment.as_deref());
    let selected_status = normalize_app_runtime_status_filter(query.status.as_deref());
    let search_query = query.q.unwrap_or_default().trim().to_owned();
    let filtered_apps = apps
        .iter()
        .filter(|app| {
            app_matches_filters(
                app,
                selected_type,
                selected_environment,
                selected_status,
                &search_query,
            )
        })
        .collect::<Vec<_>>();
    let total_count = filtered_apps.len();
    let page_size = 10usize;
    let total_pages = total_count.div_ceil(page_size).max(1);
    let page = normalize_page(query.page, total_pages);
    let page_start_index = (page - 1) * page_size;
    let page_end_index = (page_start_index + page_size).min(total_count);
    let mut rows = Vec::new();
    for app in &filtered_apps[page_start_index..page_end_index] {
        let deployment_environment = match state
            .deployment_console()
            .application_environments(app.id)
            .await
        {
            Ok(environments) => environments.into_iter().next(),
            Err(error) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
            }
        };
        let active = deployment_environment
            .as_ref()
            .and_then(|environment| environment.active_run_id);
        rows.push(AppPageRow {
            id: app.id,
            name: &app.name,
            app_key: &app.app_key,
            description: if app.description.is_empty() {
                "暂无描述"
            } else {
                &app.description
            },
            environment: app_environment_label(&app.environment),
            environment_tone: app_environment_tone(&app.environment),
            runtime_status: app_runtime_status_label(&app.runtime_status),
            runtime_status_tone: app_runtime_status_tone(&app.runtime_status),
            enabled_status: app_enabled_status_label(&app.status),
            enabled_status_tone: app_enabled_status_tone(&app.status),
            updated_at: &app.updated_at,
            latest_version: deployment_environment
                .as_ref()
                .and_then(|environment| environment.latest_version.clone())
                .unwrap_or_else(|| "尚无应用版本".to_owned()),
            deployment_status: deployment_environment
                .as_ref()
                .map(|environment| {
                    console_deployment_status_label(&environment.last_deployment_status)
                })
                .unwrap_or("等待发布"),
            deployment_status_tone: deployment_environment
                .as_ref()
                .map(|environment| {
                    console_deployment_status_tone(&environment.last_deployment_status)
                })
                .unwrap_or("neutral"),
            active_run_id: active,
            active_task_id: deployment_environment
                .as_ref()
                .and_then(|environment| environment.active_task_id),
            environment_id: deployment_environment
                .as_ref()
                .map(|environment| environment.environment_id),
            unit_count: deployment_environment
                .as_ref()
                .map(|environment| environment.unit_count)
                .unwrap_or(0),
            can_deploy: session.can(SERVICES_DEPLOY)
                && app.status != "disabled"
                && active.is_none()
                && deployment_environment
                    .as_ref()
                    .and_then(|environment| environment.latest_release_id)
                    .is_some(),
            toggle_status: app_status_toggle_value(&app.status),
            toggle_label: app_status_toggle_label(&app.status),
        });
    }
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
        selected_environment,
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
        prev_page_href: app_page_href(
            "",
            selected_environment,
            selected_status,
            &search_query,
            page - 1,
        ),
        next_page_href: app_page_href(
            "",
            selected_environment,
            selected_status,
            &search_query,
            page + 1,
        ),
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
    if let Err(err) = normalize_release_source(&form.release_source) {
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
        environment: form.environment,
        app_type: "compose".to_owned(),
        deploy_strategy: form.deploy_strategy,
        release_source: form.release_source,
        auto_queue_release: form.auto_queue_release,
        work_dir,
        compose_content: form.compose_content,
        env_content: form.env_content,
        deploy_scripts: DeployScriptSet {
            pre_deploy: form.deploy_script_pre_deploy,
            deploy: form.deploy_script_deploy,
            post_deploy: form.deploy_script_post_deploy,
            switch_traffic: form.deploy_script_switch_traffic,
            cleanup: form.deploy_script_cleanup,
        },
        health_check: match normalize_health_config(
            default_if_blank(&form.health_check_kind, "none"),
            &form.health_endpoint,
            default_i64(form.health_timeout_secs, 5),
            default_i64(form.health_expected_status, 200),
        ) {
            Ok(config) => config,
            Err(err) => return (StatusCode::BAD_REQUEST, err.message().to_owned()).into_response(),
        },
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
                    "{} 启用状态 {} -> {}",
                    change.app_name,
                    app_enabled_status_label(&change.previous_status),
                    app_enabled_status_label(&change.status)
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
        &state,
        &session,
        detail,
        None,
        app_detail_notice_message(query.notice.as_deref()),
        query.environment_id,
    )
    .await
}

async fn application_deploy_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Query(query): Query<ApplicationDeployQuery>,
) -> Response {
    if !session.can(SERVICES_DEPLOY) {
        return forbidden();
    }
    let detail = match state.apps().app_detail(app_id).await {
        Ok(detail) => detail,
        Err(error) => return app_error_response(error),
    };
    if detail.app.status == "disabled" {
        return bad_request("应用已停用，不能创建部署".to_owned());
    }
    let console = match state
        .deployment_console()
        .application_detail(app_id, Some(query.environment_id))
        .await
    {
        Ok(console) => console,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let Some(environment) = console
        .environments
        .iter()
        .find(|environment| environment.environment_id == query.environment_id)
    else {
        return (StatusCode::NOT_FOUND, "应用环境不存在").into_response();
    };
    if environment.environment_status != "ready" {
        return bad_request("环境配置尚未就绪，不能部署".to_owned());
    }
    let selected_release_id = query.app_release_id.unwrap_or_else(|| {
        console
            .releases
            .first()
            .map(|release| release.release_id)
            .unwrap_or_default()
    });
    if !console
        .releases
        .iter()
        .any(|release| release.release_id == selected_release_id)
    {
        return bad_request("应用版本不存在或不可部署".to_owned());
    }
    let mode = match query.mode.as_deref().unwrap_or("normal") {
        "normal" => DeploymentMode::Normal,
        "force" => DeploymentMode::Force,
        _ => return bad_request("部署模式必须是 normal 或 force".to_owned()),
    };
    let plan = match state
        .deployment_orchestrator()
        .preview(query.environment_id, selected_release_id, mode)
        .await
    {
        Ok(plan) => plan,
        Err(error) => return bad_request(error.to_string()),
    };
    let plan_rows = plan
        .items
        .iter()
        .map(|item| ApplicationDeployPlanRow {
            stage_no: item.stage_no,
            unit_key: item.unit_key.clone(),
            version: item
                .release_version
                .clone()
                .unwrap_or_else(|| "停用".to_owned()),
            action: deployment_action_label(item.action),
            action_tone: deployment_action_tone(item.action),
            reason: item.reason.clone(),
        })
        .collect::<Vec<_>>();
    let deploy_count = plan
        .items
        .iter()
        .filter(|item| !matches!(item.action, DeploymentAction::Skip | DeploymentAction::Stop))
        .count();
    let skip_count = plan
        .items
        .iter()
        .filter(|item| item.action == DeploymentAction::Skip)
        .count();
    let stop_count = plan
        .items
        .iter()
        .filter(|item| item.action == DeploymentAction::Stop)
        .count();
    let releases = console
        .releases
        .iter()
        .map(|release| ApplicationReleaseRow {
            id: release.release_id,
            version: release.version.clone(),
            version_code: release.version_code,
            unit_count: release.unit_count,
            created_at: release.created_at.clone(),
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/apps", &session);
    render_html(ApplicationDeployTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        app_id,
        app_name: &detail.app.name,
        environment_id: environment.environment_id,
        environment_name: &environment.environment_name,
        releases: &releases,
        selected_release_id,
        mode: if mode == DeploymentMode::Force {
            "force"
        } else {
            "normal"
        },
        mode_label: if mode == DeploymentMode::Force {
            "强制全量"
        } else {
            "正常部署"
        },
        plan_hash: &plan.plan_hash,
        plan_rows: &plan_rows,
        deploy_count,
        skip_count,
        stop_count,
        has_active_run: environment.active_run_id.is_some(),
        active_run_id: environment.active_run_id.unwrap_or_default(),
        executor_available: state.deployment_executor().is_some(),
    })
}

async fn application_deploy_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(app_id): Path<i64>,
    Form(form): Form<ApplicationDeployForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY) {
        return forbidden();
    }
    let Some(executor) = state.deployment_executor().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "部署执行器不可用，请检查配置主密钥",
        )
            .into_response();
    };
    let mode = match form.mode.as_str() {
        "normal" => DeploymentMode::Normal,
        "force" => DeploymentMode::Force,
        _ => return bad_request("部署模式必须是 normal 或 force".to_owned()),
    };
    let environment_app_id: Option<i64> = match sqlx::query_scalar(
        "SELECT app_id FROM app_environments WHERE id = ?1 AND status = 'ready'",
    )
    .bind(form.environment_id)
    .fetch_optional(state.db())
    .await
    {
        Ok(app_id) => app_id,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    if environment_app_id != Some(app_id) {
        return bad_request("应用环境不存在或尚未就绪".to_owned());
    }
    let created = match state
        .deployment_orchestrator()
        .create_run(CreateDeploymentRunInput {
            environment_id: form.environment_id,
            app_release_id: form.app_release_id,
            mode,
            expected_plan_hash: form.expected_plan_hash,
            created_by: session.account.username.clone(),
        })
        .await
    {
        Ok(created) => created,
        Err(error) => return deployment_orchestrator_error_response(error),
    };
    let deployment_run_id = created.deployment_run_id;
    let task_id = created.task_id;
    let orchestrator = state.deployment_orchestrator().clone();
    let cancellations = state.deployment_cancellations().clone();
    let cancellation = cancellations.register(deployment_run_id);
    tokio::spawn(async move {
        if let Err(error) = orchestrator
            .execute_run_with_cancellation(deployment_run_id, executor, cancellation)
            .await
        {
            tracing::error!(deployment_run_id, error = %error, "environment deployment execution failed");
            if let Err(finalize_error) = orchestrator
                .fail_run_on_internal_error(deployment_run_id, &error.to_string())
                .await
            {
                tracing::error!(deployment_run_id, error = %finalize_error, "failed to finalize broken environment deployment");
            }
        }
        cancellations.remove(deployment_run_id);
    });
    redirect(&format!("/tasks/{task_id}"))
}

async fn render_app_detail(
    state: &AppState,
    session: &CurrentSession,
    detail: crate::apps::AppConfigDetail,
    compose_result: Option<ComposeResultView>,
    notice: Option<&str>,
    selected_environment_id: Option<i64>,
) -> Response {
    let console = match state
        .deployment_console()
        .application_detail(detail.app.id, selected_environment_id)
        .await
    {
        Ok(console) => console,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let selected_environment_id = selected_environment_id
        .filter(|id| {
            console
                .environments
                .iter()
                .any(|environment| environment.environment_id == *id)
        })
        .or_else(|| {
            console
                .environments
                .first()
                .map(|environment| environment.environment_id)
        })
        .unwrap_or_default();
    let deployment_environments = console
        .environments
        .iter()
        .map(|environment| DeploymentEnvironmentRow {
            id: environment.environment_id,
            name: environment.environment_name.clone(),
            key: environment.environment_key.clone(),
            status: environment_status_label(&environment.environment_status),
            status_tone: environment_status_tone(&environment.environment_status),
            runtime_status: environment_runtime_status_label(&environment.runtime_status),
            runtime_tone: environment_runtime_status_tone(&environment.runtime_status),
            latest_version: environment
                .latest_version
                .clone()
                .unwrap_or_else(|| "尚无应用版本".to_owned()),
            target_count: environment.target_count,
            active_run_id: environment.active_run_id,
            active_task_id: environment.active_task_id,
            active_run_status: environment.active_run_status.clone().unwrap_or_default(),
            selected: environment.environment_id == selected_environment_id,
        })
        .collect::<Vec<_>>();
    let deployment_units = console
        .units
        .iter()
        .map(|unit| {
            let (runtime_status, runtime_tone) = unit_runtime_summary(unit);
            DeploymentUnitRow {
                key: unit.unit_key.clone(),
                name: unit.unit_name.clone(),
                stage: format!("阶段 {} · {}", unit.stage_no, unit.stage_name),
                lifecycle_status: if unit.lifecycle_status == "active" {
                    "启用"
                } else {
                    "停用"
                },
                lifecycle_tone: if unit.lifecycle_status == "active" {
                    "success"
                } else {
                    "neutral"
                },
                latest_version: unit
                    .latest_version
                    .clone()
                    .unwrap_or_else(|| "尚无发布包".to_owned()),
                runtime_status,
                runtime_tone,
                work_dir: unit.work_dir.clone(),
            }
        })
        .collect::<Vec<_>>();
    let application_releases = console
        .releases
        .iter()
        .map(|release| ApplicationReleaseRow {
            id: release.release_id,
            version: release.version.clone(),
            version_code: release.version_code,
            unit_count: release.unit_count,
            created_at: release.created_at.clone(),
        })
        .collect::<Vec<_>>();
    let environment_runs = console
        .runs
        .iter()
        .map(|run| EnvironmentDeploymentRunRow {
            id: run.run_id,
            task_id: run.task_id,
            environment_name: run.environment_name.clone(),
            version: run.release_version.clone(),
            mode: if run.deployment_mode == "force" {
                "强制全量"
            } else {
                "正常部署"
            },
            status: console_deployment_status_label(&run.status),
            status_tone: console_deployment_status_tone(&run.status),
            result_summary: format!(
                "{} 成功 · {} 失败 · {} 跳过 · {} 未完成",
                run.success_count, run.failed_count, run.skipped_count, run.pending_count
            ),
            summary: if run.summary.is_empty() {
                "暂无执行摘要".to_owned()
            } else {
                run.summary.clone()
            },
            created_at: run.created_at.clone(),
        })
        .collect::<Vec<_>>();
    let nav_sections = nav_sections("/apps", session);
    let app_enabled = detail.app.status != "disabled";
    let app_idle = !app_detail_is_deploying(&detail);
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
            config_revision: if run.config_revision_no > 0 {
                format!("config#{}", run.config_revision_no)
            } else {
                "未记录配置版本".to_owned()
            },
            artifact_version: display_text(run.artifact_version.clone(), "无发布版本"),
            started_at: run.started_at.clone(),
            finished_at: run
                .finished_at
                .clone()
                .unwrap_or_else(|| "未结束".to_owned()),
        })
        .collect::<Vec<_>>();
    let can_manage = session.can("apps.update") && app_enabled && app_idle;
    let config_snapshots = detail
        .config_snapshots
        .iter()
        .map(|snapshot| AppConfigSnapshotRow {
            id: snapshot.id,
            revision: format!("config#{}", snapshot.revision_no),
            kind: snapshot_kind_label(&snapshot.snapshot_kind),
            compose_summary: config_summary(&snapshot.compose_content),
            env_summary: config_summary(&snapshot.env_content),
            artifact_version: display_text(snapshot.artifact_version.clone(), "无发布版本"),
            config_hash: short_hash(&snapshot.config_hash),
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
        environment: &detail.app.environment,
        environment_label: app_environment_label(&detail.app.environment),
        environment_tone: app_environment_tone(&detail.app.environment),
        deploy_strategy: detail.app.deploy_strategy.as_str(),
        deploy_strategy_label: deploy_strategy_label(&detail.app.deploy_strategy),
        release_source: &detail.app.release_source,
        release_source_label: release_source_label(&detail.app.release_source),
        auto_queue_release: detail.app.auto_queue_release == 1,
        release_publish_mode: release_publish_mode_label(detail.app.auto_queue_release == 1),
        work_dir: &detail.app.work_dir,
        runtime_root: &detail.runtime_root,
        status: app_enabled_status_label(&detail.app.status),
        status_tone: app_enabled_status_tone(&detail.app.status),
        targets: detail.app.target_names.as_deref().unwrap_or("未绑定节点"),
        target_count: detail.app.target_count,
        created_at: &detail.app.created_at,
        updated_at: &detail.app.updated_at,
        compose_content: &detail.compose_content,
        env_content: &detail.env_content,
        deploy_script_pre_deploy: &detail.deploy_scripts.pre_deploy,
        deploy_script_deploy: &detail.deploy_scripts.deploy,
        deploy_script_post_deploy: &detail.deploy_scripts.post_deploy,
        deploy_script_switch_traffic: &detail.deploy_scripts.switch_traffic,
        deploy_script_cleanup: &detail.deploy_scripts.cleanup,
        metadata_content: &detail.metadata_content,
        health_check_kind: detail.health_check.kind.as_str(),
        health_check_label: detail.health_check.kind.label(),
        health_endpoint: &detail.health_check.endpoint,
        health_timeout_secs: detail.health_check.timeout_secs,
        health_expected_status: detail.health_check.expected_status,
        deployment_runs: &deployment_runs,
        deployment_environments: &deployment_environments,
        deployment_units: &deployment_units,
        application_releases: &application_releases,
        environment_runs: &environment_runs,
        selected_environment_id,
        config_snapshots: &config_snapshots,
        deploy_diff: &deploy_diff,
        runtime_states: &runtime_states,
        target_choices: &target_choices,
        can_manage,
        can_deploy: session.can(SERVICES_DEPLOY) && app_enabled && app_idle,
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
        deploy_scripts: DeployScriptSet {
            pre_deploy: form.deploy_script_pre_deploy,
            deploy: form.deploy_script_deploy,
            post_deploy: form.deploy_script_post_deploy,
            switch_traffic: form.deploy_script_switch_traffic,
            cleanup: form.deploy_script_cleanup,
        },
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
    if let Err(err) = normalize_release_source(&form.release_source) {
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
        environment: form.environment,
        work_dir: form.work_dir,
        deploy_strategy: form.deploy_strategy,
        release_source: form.release_source,
        auto_queue_release: form.auto_queue_release,
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
    if !session.can(SERVICES_DEPLOY) {
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
    render_app_detail(
        &state,
        &session,
        detail,
        Some(compose_result_view(command)),
        None,
        None,
    )
    .await
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
    render_app_detail(
        &state,
        &session,
        detail,
        Some(compose_result_view(command)),
        None,
        None,
    )
    .await
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

async fn artifact_upload_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    multipart: Multipart,
) -> Response {
    if !session.can(ARTIFACTS_UPLOAD) {
        return forbidden();
    }
    let input = match parse_artifact_upload_multipart(&session, multipart).await {
        Ok(input) => input,
        Err(response) => return response,
    };
    let app_id = input.app_id;
    match state.apps().upload_release_package(input).await {
        Ok(_) => {
            record_audit_event(
                &state,
                &session,
                "artifacts.upload",
                "app",
                &app_id.to_string(),
                "上传发布版本包",
            )
            .await;
            redirect("/artifacts?source=upload")
        }
        Err(err) => app_error_response(err),
    }
}

async fn artifact_publish_now_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ReleasePublishNowForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY) {
        return forbidden();
    }
    match state
        .apps()
        .publish_release_now(form.release_id, &session.account.username)
        .await
    {
        Ok(queue_id) => {
            record_audit_event(
                &state,
                &session,
                "services.deploy",
                "release",
                &form.release_id.to_string(),
                &format!(
                    "手动发布版本，{}",
                    queue_id
                        .map(|id| format!("队列项 #{id}"))
                        .unwrap_or_else(|| "已存在排队项".to_owned())
                ),
            )
            .await;
            redirect("/artifacts")
        }
        Err(AppError::Conflict(message)) if message == APP_DEPLOYMENT_IN_PROGRESS_MESSAGE => {
            redirect("/artifacts?notice=app-deploying")
        }
        Err(err) => app_error_response(err),
    }
}

async fn artifact_schedule_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ReleaseScheduleForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY) {
        return forbidden();
    }
    match state
        .apps()
        .schedule_release_publish(form.release_id, &form.scheduled_publish_at)
        .await
    {
        Ok(scheduled_at) => {
            record_audit_event(
                &state,
                &session,
                "services.deploy",
                "release",
                &form.release_id.to_string(),
                &format!("设置计划发布时间 {scheduled_at}"),
            )
            .await;
            redirect("/artifacts")
        }
        Err(err) => app_error_response(err),
    }
}

async fn artifact_schedule_cancel_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ReleaseCancelScheduleForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY) {
        return forbidden();
    }
    match state.apps().cancel_scheduled_release(form.release_id).await {
        Ok(()) => {
            record_audit_event(
                &state,
                &session,
                "services.deploy",
                "release",
                &form.release_id.to_string(),
                "取消计划发布时间",
            )
            .await;
            redirect("/artifacts")
        }
        Err(err) => app_error_response(err),
    }
}

async fn artifact_queue_cancel_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ReleaseQueueCancelForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY) {
        return forbidden();
    }
    match state.apps().cancel_release_queue_item(form.queue_id).await {
        Ok(_) => {
            record_audit_event(
                &state,
                &session,
                "services.deploy",
                "release_queue",
                &form.queue_id.to_string(),
                "取消等待中的发布队列项",
            )
            .await;
            redirect("/artifacts")
        }
        Err(err) => app_error_response(err),
    }
}

fn app_detail_is_deploying(detail: &crate::apps::AppConfigDetail) -> bool {
    detail.app.status == "deploying"
        || detail
            .runtime_states
            .iter()
            .any(|state| state.runtime_status == "deploying")
}

#[derive(Clone, Copy)]
enum DeployConfirmAction {
    Compose(ComposeTaskAction),
}

async fn render_deploy_confirm(
    state: &AppState,
    session: &CurrentSession,
    app_id: i64,
    action: DeployConfirmAction,
) -> Response {
    if !session.can(SERVICES_DEPLOY) {
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
    if app_detail_is_deploying(&detail) {
        return app_error_response(AppError::Conflict(
            "应用正在部署中，请等待当前任务结束".to_owned(),
        ));
    }
    if detail.app.target_count <= 0 {
        return app_error_response(AppError::InvalidInput(
            "应用没有可用目标节点，请先启用节点或调整目标节点".to_owned(),
        ));
    }
    if detail.app.app_type != "compose" {
        return app_error_response(AppError::InvalidInput(
            "当前应用不是 Compose 应用".to_owned(),
        ));
    }

    let nav_sections = nav_sections("/apps", session);
    let deploy_diff = deploy_diff_view(&detail.deploy_diff);
    let DeployConfirmAction::Compose(compose_action) = action;
    let post_action = compose_submit_path(app_id, compose_action);
    let action_label = deploy_confirm_action_label(action);
    let action_tone = deploy_confirm_action_tone(action);
    let action_description = deploy_confirm_action_description(action);
    let targets = detail.app.target_names.as_deref().unwrap_or("未绑定节点");
    let deploy_strategy = deploy_strategy_label(&detail.app.deploy_strategy);
    let health_endpoint = display_text(detail.health_check.endpoint.clone(), "未配置");
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
    if !session.can(SERVICES_DEPLOY) {
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

async fn parse_artifact_upload_multipart(
    session: &CurrentSession,
    mut multipart: Multipart,
) -> Result<UploadReleasePackageInput, Response> {
    let mut csrf_token = String::new();
    let mut app_id = None;
    let mut artifact_version = String::new();
    let mut version_code = None;
    let mut published_at = String::new();
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
            "app_id" => {
                let value = field.text().await.map_err(|err| {
                    (StatusCode::BAD_REQUEST, format!("读取应用字段失败: {err}")).into_response()
                })?;
                app_id =
                    Some(value.trim().parse::<i64>().map_err(|_| {
                        (StatusCode::BAD_REQUEST, "请选择关联应用").into_response()
                    })?);
            }
            "artifact_version" => {
                artifact_version = field.text().await.map_err(|err| {
                    (StatusCode::BAD_REQUEST, format!("读取版本字段失败: {err}")).into_response()
                })?;
            }
            "version_code" | "versionCode" => {
                let value = field.text().await.map_err(|err| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("读取 versionCode 字段失败: {err}"),
                    )
                        .into_response()
                })?;
                version_code = parse_optional_i64(&value)
                    .map_err(|message| (StatusCode::BAD_REQUEST, message).into_response())?;
            }
            "published_at" | "publishedAt" => {
                published_at = field.text().await.map_err(|err| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("读取发布时间字段失败: {err}"),
                    )
                        .into_response()
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
    Ok(UploadReleasePackageInput {
        app_id: app_id
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "请选择关联应用").into_response())?,
        release_version: artifact_version,
        version_code,
        published_at,
        file_name,
        bytes,
        entry_file,
        source: "upload".to_owned(),
    })
}

async fn parse_api_package_upload_multipart(
    mut multipart: Multipart,
) -> Result<ApiPackageUploadInput, Response> {
    let mut release_version = String::new();
    let mut version_code = None;
    let mut published_at = String::new();
    let mut entry_file = String::new();
    let mut source = String::new();
    let mut file_name = String::new();
    let mut bytes = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|err| {
        api_error(
            StatusCode::BAD_REQUEST,
            &format!("读取版本包上传表单失败: {err}"),
        )
    })? {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "release_version" | "artifact_version" => {
                release_version = field.text().await.map_err(|err| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("读取发布版本字段失败: {err}"),
                    )
                })?;
            }
            "entry_file" => {
                entry_file = field.text().await.map_err(|err| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("读取入口文件字段失败: {err}"),
                    )
                })?;
            }
            "version_code" | "versionCode" => {
                let value = field.text().await.map_err(|err| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("读取 versionCode 字段失败: {err}"),
                    )
                })?;
                version_code = parse_optional_i64(&value)
                    .map_err(|message| api_error(StatusCode::BAD_REQUEST, &message))?;
            }
            "published_at" | "publishedAt" => {
                published_at = field.text().await.map_err(|err| {
                    api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("读取发布时间字段失败: {err}"),
                    )
                })?;
            }
            "source" => {
                source = field.text().await.map_err(|err| {
                    api_error(StatusCode::BAD_REQUEST, &format!("读取来源字段失败: {err}"))
                })?;
            }
            "package_file" | "file" | "artifact_file" => {
                file_name = field
                    .file_name()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| "package.bin".to_owned());
                bytes = field
                    .bytes()
                    .await
                    .map_err(|err| {
                        api_error(
                            StatusCode::BAD_REQUEST,
                            &format!("读取版本包文件失败: {err}"),
                        )
                    })?
                    .to_vec();
            }
            _ => {}
        }
    }
    if file_name.trim().is_empty() || bytes.is_empty() {
        return Err(api_error(StatusCode::BAD_REQUEST, "请上传版本包文件"));
    }
    Ok(ApiPackageUploadInput {
        artifact_version: release_version,
        version_code,
        published_at,
        file_name,
        bytes,
        entry_file,
        source,
    })
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
    let selected_kind = "compose";
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
                app_status: app_enabled_status_label(&service.app_status),
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
    if detail.app.app_type != "compose" {
        return app_error_response(AppError::InvalidInput(
            "业务应用级二进制日志入口已停用，请使用 Compose 应用日志".to_owned(),
        ));
    }
    let log_output = match state
        .apps()
        .compose_service_logs(app_id, &service_name, query.node_id, tail_lines)
        .await
    {
        Ok(output) => output,
        Err(err) => return app_error_response(err),
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
        .map(|node| node_page_row_clean(node, can_manage_nodes))
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
                .map(node_check_history_row_clean)
                .collect::<Vec<_>>(),
            apps: detail
                .apps
                .iter()
                .map(node_app_runtime_row_clean)
                .collect::<Vec<_>>(),
            tasks: detail
                .tasks
                .iter()
                .map(node_task_row_clean)
                .collect::<Vec<_>>(),
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
    let node_row = node_page_row_clean(node, session.can(NODES_MANAGE));
    let checks = detail
        .checks
        .iter()
        .map(node_check_history_row_clean)
        .collect::<Vec<_>>();
    let apps = detail
        .apps
        .iter()
        .map(node_app_runtime_row_clean)
        .collect::<Vec<_>>();
    let tasks = detail
        .tasks
        .iter()
        .map(node_task_row_clean)
        .collect::<Vec<_>>();
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
                    node_status_label_clean(&change.previous_status),
                    node_status_label_clean(&change.status)
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
            status: node_check_result_node_status_label_clean(&result.status),
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
                    .unwrap_or_else(|| "-".to_owned()),
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
    let deployment_control = match state
        .deployment_orchestrator()
        .run_control_for_task(task_id)
        .await
    {
        Ok(Some(control)) => {
            let cancel_requested = control.cancel_requested_at.is_some();
            let cancel_in_progress =
                cancel_requested && matches!(control.status.as_str(), "queued" | "running");
            DeploymentTaskControlView {
                has_run: true,
                run_id: control.deployment_run_id,
                status: console_deployment_status_label(&control.status),
                status_tone: console_deployment_status_tone(&control.status),
                cancel_in_progress,
                cancel_requested_by: control.cancel_requested_by,
                cancel_requested_at: control.cancel_requested_at.unwrap_or_default(),
                show_cancel_action: session.can(SERVICES_DEPLOY_CANCEL)
                    && matches!(control.status.as_str(), "queued" | "running")
                    && !cancel_requested,
                show_reconcile_action: session.can(SERVICES_DEPLOY_RECONCILE)
                    && control.status == "reconciling",
            }
        }
        Ok(None) => DeploymentTaskControlView::default(),
        Err(error) => return deployment_orchestrator_error_response(error),
    };
    let logs = match state
        .tasks()
        .task_logs_with_deployment(task_id, state.deployment_logs())
        .await
    {
        Ok(logs) => logs,
        Err(err) => return task_error_response(err),
    };
    let steps = match state.tasks().task_steps(task_id).await {
        Ok(steps) => steps,
        Err(err) => return task_error_response(err),
    };
    let task_phases = match state.tasks().task_phases(task_id).await {
        Ok(phases) => phases,
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
            .unwrap_or_else(|| "-".to_owned()),
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
    let mut logs_by_step = HashMap::<i64, Vec<TaskLogRow<'_>>>::new();
    let mut ungrouped_log_rows = Vec::new();
    for log in &logs {
        let row = TaskLogRow {
            source_label: log.source.label(),
            stream: &log.stream,
            stream_tone: task_log_stream_tone(&log.stream),
            content: &log.content,
            created_at: &log.created_at,
        };
        if let Some(step_id) = log.step_id {
            logs_by_step.entry(step_id).or_default().push(row);
        } else {
            ungrouped_log_rows.push(row);
        }
    }
    let step_rows = steps
        .iter()
        .map(|step| {
            let logs = logs_by_step.remove(&step.id).unwrap_or_default();
            let has_logs = !logs.is_empty();
            TaskStepRow {
                step_no: step.step_no,
                title: &step.title,
                node_name: step.node_name.as_deref().unwrap_or("全局"),
                status: task_step_status_label(&step.status),
                status_tone: task_step_status_tone(&step.status),
                command: task_display_text(&step.command, "无命令"),
                exit_code: step
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                started_at: step.started_at.as_deref().unwrap_or("未开始"),
                finished_at: step.finished_at.as_deref().unwrap_or("未结束"),
                logs,
                has_logs,
                is_open: matches!(step.status.as_str(), "running" | "failed"),
            }
        })
        .collect::<Vec<_>>();
    let mut step_rows_by_phase = HashMap::<Option<i64>, Vec<TaskStepRow<'_>>>::new();
    for (step, row) in steps.iter().zip(step_rows.iter()) {
        step_rows_by_phase
            .entry(step.phase_id)
            .or_default()
            .push(row.clone());
    }
    let mut phase_groups = Vec::new();
    for phase in &task_phases {
        let phase_steps = step_rows_by_phase
            .remove(&Some(phase.id))
            .unwrap_or_default();
        let has_steps = !phase_steps.is_empty();
        let step_is_open = phase_steps.iter().any(|step| step.is_open);
        let title = if phase.title.trim().is_empty() {
            task_phase_label(&phase.phase_key).to_owned()
        } else {
            phase.title.clone()
        };
        phase_groups.push(TaskPhaseGroupRow {
            phase_no: phase.phase_no,
            title,
            phase_key: phase.phase_key.clone(),
            status: task_step_status_label(&phase.status),
            status_tone: task_step_status_tone(&phase.status),
            summary: task_display_text(&phase.summary, "暂无阶段摘要").to_owned(),
            started_at: phase.started_at.as_deref().unwrap_or("未开始").to_owned(),
            finished_at: phase.finished_at.as_deref().unwrap_or("未结束").to_owned(),
            steps: phase_steps,
            has_steps,
            is_open: matches!(phase.status.as_str(), "running" | "failed")
                || phase.phase_key == task.phase
                || step_is_open,
        });
    }
    let mut ungrouped_steps = step_rows_by_phase.remove(&None).unwrap_or_default();
    for (_, mut remaining_steps) in step_rows_by_phase {
        ungrouped_steps.append(&mut remaining_steps);
    }
    if !ungrouped_steps.is_empty() {
        let step_is_open = ungrouped_steps.iter().any(|step| step.is_open);
        phase_groups.push(TaskPhaseGroupRow {
            phase_no: 0,
            title: "未归档步骤".to_owned(),
            phase_key: "ungrouped".to_owned(),
            status: "已记录",
            status_tone: "neutral",
            summary: "历史任务或未绑定阶段的步骤".to_owned(),
            started_at: "-".to_owned(),
            finished_at: "-".to_owned(),
            steps: ungrouped_steps,
            has_steps: true,
            is_open: step_is_open || task.status == "failed",
        });
    }
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
        deployment_control,
        execution_guide,
        return_action,
        phases: &phase_rows,
        phase_groups: &phase_groups,
        node_results: &node_result_rows,
        logs: &ungrouped_log_rows,
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
    let deployment_control = match state
        .deployment_orchestrator()
        .run_control_for_task(task_id)
        .await
    {
        Ok(control) => control,
        Err(error) => return deployment_orchestrator_error_response(error),
    };
    if let Some(control) = deployment_control {
        if !session.can(SERVICES_DEPLOY_CANCEL) {
            return forbidden();
        }
        let requested = match state
            .deployment_orchestrator()
            .request_cancellation(control.deployment_run_id, &session.account.username)
            .await
        {
            Ok(requested) => requested,
            Err(error) => return deployment_orchestrator_error_response(error),
        };
        state
            .deployment_cancellations()
            .cancel(control.deployment_run_id);
        if requested {
            record_audit_event(
                &state,
                &session,
                SERVICES_DEPLOY_CANCEL,
                "environment_deployment_run",
                &control.deployment_run_id.to_string(),
                "通过任务详情请求取消环境部署",
            )
            .await;
        }
        return redirect(&format!("/tasks/{task_id}"));
    }
    if !session.can("tasks.cancel") {
        return forbidden();
    }
    match state
        .apps()
        .cancel_queued_task(task_id, &session.account.username)
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
        Err(err) => app_error_response(err),
    }
}

async fn deployment_cancel_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(deployment_run_id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY_CANCEL) {
        return forbidden();
    }
    let task_id = match deployment_task_id(state.db(), deployment_run_id).await {
        Ok(task_id) => task_id,
        Err(response) => return response,
    };
    let requested = match state
        .deployment_orchestrator()
        .request_cancellation(deployment_run_id, &session.account.username)
        .await
    {
        Ok(requested) => requested,
        Err(error) => return deployment_orchestrator_error_response(error),
    };
    state.deployment_cancellations().cancel(deployment_run_id);
    if requested {
        record_audit_event(
            &state,
            &session,
            SERVICES_DEPLOY_CANCEL,
            "environment_deployment_run",
            &deployment_run_id.to_string(),
            "请求取消运行中的环境部署",
        )
        .await;
    }
    redirect(&format!("/tasks/{task_id}"))
}

async fn deployment_confirm_stopped_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(deployment_run_id): Path<i64>,
    Form(form): Form<DeploymentReconciliationForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(SERVICES_DEPLOY_RECONCILE) {
        return forbidden();
    }
    let task_id = match deployment_task_id(state.db(), deployment_run_id).await {
        Ok(task_id) => task_id,
        Err(response) => return response,
    };
    if let Err(error) = state
        .deployment_orchestrator()
        .confirm_interrupted_run_stopped(deployment_run_id, &session.account.username, &form.note)
        .await
    {
        return deployment_orchestrator_error_response(error);
    }
    let message = format!("确认中断部署的远端执行已停止：{}", form.note.trim());
    record_audit_event(
        &state,
        &session,
        SERVICES_DEPLOY_RECONCILE,
        "environment_deployment_run",
        &deployment_run_id.to_string(),
        &message,
    )
    .await;
    redirect(&format!("/tasks/{task_id}"))
}

async fn deployment_task_id(db: &SqlitePool, deployment_run_id: i64) -> Result<i64, Response> {
    match sqlx::query_scalar::<_, i64>(
        "SELECT task_id FROM environment_deployment_runs WHERE id = ?1 AND task_id IS NOT NULL",
    )
    .bind(deployment_run_id)
    .fetch_optional(db)
    .await
    {
        Ok(Some(task_id)) => Ok(task_id),
        Ok(None) => Err((StatusCode::NOT_FOUND, "部署执行不存在").into_response()),
        Err(error) => Err((StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()),
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

async fn templates_page(session: CurrentSession) -> Response {
    if !session.can(TEMPLATES_VIEW) {
        return forbidden();
    }
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
    })
}

async fn artifacts_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<ArtifactsQuery>,
) -> Response {
    if !session.can(ARTIFACTS_VIEW) {
        return forbidden();
    }
    let releases = match state.apps().list_app_releases().await {
        Ok(releases) => releases,
        Err(err) => return app_error_response(err),
    };
    let release_queue = match state.apps().list_app_release_queue().await {
        Ok(queue) => queue,
        Err(err) => return app_error_response(err),
    };
    let mut deploying_app_ids = BTreeSet::new();
    for app_id in releases
        .iter()
        .map(|release| release.app_id)
        .collect::<BTreeSet<_>>()
    {
        match state.tasks().active_app_task(app_id).await {
            Ok(Some(task)) if task.status == "running" => {
                deploying_app_ids.insert(app_id);
            }
            Ok(_) => {}
            Err(err) => return task_error_response(err),
        }
    }
    let apps = match state.apps().list_apps().await {
        Ok(apps) => apps,
        Err(err) => return app_error_response(err),
    };
    let package_apps = apps
        .iter()
        .filter(|app| {
            app.app_type == "compose"
                && app.release_source == "package_upload"
                && app.status != "disabled"
        })
        .map(|app| ArtifactAppOptionRow {
            id: app.id,
            label: app.name.clone(),
            detail: format!(
                "{} · {} · {}",
                app.app_key,
                app.work_dir,
                release_publish_mode_label(app.auto_queue_release == 1)
            ),
        })
        .collect::<Vec<_>>();
    let queued_count = releases
        .iter()
        .filter(|release| matches!(release.status.as_str(), "queued" | "deploying"))
        .count();
    let uploaded_count = releases
        .iter()
        .filter(|release| matches!(release.source.as_str(), "openapi" | "web"))
        .count();
    let latest_time = releases
        .first()
        .map(|release| format_datetime_shanghai(&release.published_at))
        .unwrap_or_else(|| "暂无发布版本".to_owned());
    let selected_status = normalize_artifact_status_filter(query.status.as_deref());
    let selected_kind = normalize_artifact_kind_filter(query.kind.as_deref());
    let selected_source = normalize_artifact_source_filter(query.source.as_deref());
    let search_query = query.q.as_deref().unwrap_or_default().trim().to_owned();
    let notice = artifact_notice_message(query.notice.as_deref());
    let platform_config = match state.platform().config().await {
        Ok(config) => config,
        Err(err) => return platform_error_response(err),
    };
    let filtered_releases = releases
        .iter()
        .filter(|release| {
            if !selected_status.is_empty() && release.status != selected_status {
                return false;
            }
            if !selected_kind.is_empty() {
                let kind = if release.package_name.ends_with(".tar.gz")
                    || release.package_name.ends_with(".tgz")
                {
                    "tar_gz"
                } else {
                    "binary"
                };
                if kind != selected_kind {
                    return false;
                }
            }
            if !selected_source.is_empty() {
                let source = match release.source.as_str() {
                    "openapi" => "openapi",
                    "web" => "web",
                    "initial" => "initial",
                    _ => "",
                };
                if selected_source == "upload" {
                    if !matches!(source, "web" | "openapi") {
                        return false;
                    }
                } else if source != selected_source {
                    return false;
                }
            }
            if search_query.is_empty() {
                return true;
            }
            let haystack = format!(
                "{} {} {} {} {} {} {} {}",
                release.app_name,
                release.app_key,
                release.version,
                release.package_name,
                release.package_path,
                release.storage_bucket,
                release.storage_object_key,
                release.checksum_sha256
            )
            .to_lowercase();
            haystack.contains(&search_query.to_lowercase())
        })
        .collect::<Vec<_>>();
    let rows = filtered_releases
        .iter()
        .map(|release| {
            let app_deploying = deploying_app_ids.contains(&release.app_id);
            let kind = if release.package_name.ends_with(".tar.gz")
                || release.package_name.ends_with(".tgz")
            {
                "tar_gz"
            } else {
                "binary"
            };
            let active_queue = release_queue.iter().find(|item| {
                item.release_id == release.id
                    && matches!(item.status.as_str(), "scheduled" | "queued" | "running")
            });
            let publish_mode = apps
                .iter()
                .find(|app| app.id == release.app_id)
                .map(|app| release_publish_mode_label(app.auto_queue_release == 1))
                .unwrap_or("手动发布");
            let storage_detail = if release.storage_provider == "aliyun_oss" {
                format!("{}/{}", release.storage_bucket, release.storage_object_key)
            } else {
                release.package_path.clone()
            };
            ArtifactPageRow {
                id: release.id,
                app_id: release.app_id,
                app_name: release.app_name.clone(),
                app_key: release.app_key.clone(),
                version: release.version.clone(),
                version_code: release.version_code,
                artifact_kind: artifact_kind_label(kind).to_owned(),
                status: release_status_label(&release.status),
                status_tone: release_status_tone(&release.status),
                queue_status: active_queue
                    .map(|item| queue_status_label(&item.status).to_owned())
                    .unwrap_or_else(|| "未排队".to_owned()),
                queue_status_tone: active_queue
                    .map(|item| queue_status_tone(&item.status))
                    .unwrap_or("neutral"),
                publish_mode,
                storage: storage_provider_label(&release.storage_provider).to_owned(),
                storage_detail,
                sha256: short_hash(&release.checksum_sha256),
                size: format_size(&release.size_bytes.to_string()),
                entry_file: display_text(
                    artifact_metadata_value(&release.metadata, "entry_file"),
                    "未记录",
                ),
                source: artifact_source_label(&release.source).to_owned(),
                published_at: format_datetime_shanghai(&release.published_at),
                received_at: format_datetime_shanghai(&release.received_at),
                scheduled_publish_at: display_text(
                    active_queue
                        .and_then(|item| item.scheduled_publish_at.as_deref())
                        .map(format_datetime_shanghai)
                        .unwrap_or_default(),
                    "未设置",
                ),
                scheduled_publish_input: format_datetime_local_input(
                    active_queue
                        .and_then(|item| item.scheduled_publish_at.as_deref())
                        .unwrap_or_default(),
                ),
                queue_id: active_queue.map(|item| item.id),
                task_id: active_queue.and_then(|item| item.task_id),
                can_publish_now: session.can(SERVICES_DEPLOY)
                    && active_queue.is_none()
                    && !app_deploying
                    && !matches!(release.status.as_str(), "deploying"),
                app_deploying,
                can_schedule: session.can(SERVICES_DEPLOY)
                    && active_queue.is_none()
                    && !matches!(release.status.as_str(), "deploying"),
                can_cancel_schedule: session.can(SERVICES_DEPLOY)
                    && active_queue.is_some_and(|item| item.status == "scheduled"),
                can_cancel_queue: session.can(SERVICES_DEPLOY)
                    && active_queue.is_some_and(|item| item.status == "queued"),
            }
        })
        .collect::<Vec<_>>();
    let queue_rows = release_queue
        .iter()
        .map(|item| ReleaseQueueRow {
            id: item.id,
            app_id: item.app_id,
            app_name: item.app_name.clone(),
            app_key: item.app_key.clone(),
            version: item.version.clone(),
            version_code: item.version_code,
            status: queue_status_label(&item.status),
            status_tone: queue_status_tone(&item.status),
            queue_seq: item.queue_seq,
            triggered_by: item.triggered_by.clone(),
            message: item.message.clone(),
            task_id: item.task_id,
            scheduled_publish_at: item
                .scheduled_publish_at
                .as_deref()
                .map(format_datetime_shanghai)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "立即".to_owned()),
            created_at: format_datetime_shanghai(&item.created_at),
            started_at: item
                .started_at
                .as_deref()
                .map(format_datetime_shanghai)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "未开始".to_owned()),
            finished_at: item
                .finished_at
                .as_deref()
                .map(format_datetime_shanghai)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "未结束".to_owned()),
            can_cancel: session.can(SERVICES_DEPLOY) && item.status == "queued",
        })
        .collect::<Vec<_>>();
    let summary_items = vec![
        SummaryItem {
            label: "发布版本",
            value: releases.len().to_string(),
            detail: "统一版本中心，展示最近 100 条发布版本".to_owned(),
            tone: "neutral",
        },
        SummaryItem {
            label: "待发布",
            value: queued_count.to_string(),
            detail: "包含等待入队和正在发布中的版本".to_owned(),
            tone: if queued_count > 0 {
                "active"
            } else {
                "neutral"
            },
        },
        SummaryItem {
            label: "上传版本包",
            value: uploaded_count.to_string(),
            detail: "来自 OpenAPI 的版本包".to_owned(),
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
        release_queue: &queue_rows,
        package_apps: &package_apps,
        selected_status,
        selected_kind,
        selected_source,
        query: &search_query,
        notice,
        uploaded_binary_releases_to_keep: platform_config.uploaded_binary_releases_to_keep,
        can_upload: session.can(ARTIFACTS_UPLOAD),
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
            detail: "应用运行文件、release 与 current 指针根目录".to_owned(),
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
            detail: "Access/Refresh Token 的服务端会话索引，服务重启后需要重新登录".to_owned(),
            tone: "active",
        },
    ];
    let runtime_rows = vec![
        SettingsRow {
            label: "服务绑定",
            value: settings.bind.to_string(),
            detail: "来自 EASY_DEPLOY_BIND / --bind",
        },
        SettingsRow {
            label: "面板版本",
            value: concat!("v", env!("CARGO_PKG_VERSION")).to_owned(),
            detail: "当前 api 模块版本",
        },
        SettingsRow {
            label: "资源版本",
            value: ASSET_VERSION.to_owned(),
            detail: "用于刷新 CSS、logo 与 favicon 缓存",
        },
    ];
    let storage_rows = vec![
        SettingsRow {
            label: "数据目录",
            value: data_dir,
            detail: "来自 EASY_DEPLOY_DATA_DIR / --data-dir",
        },
        SettingsRow {
            label: "应用目录",
            value: apps_dir,
            detail: "每个应用会在此目录下生成 compose.yaml、.env、release 等文件",
        },
        SettingsRow {
            label: "数据库地址",
            value: settings.database_url.clone(),
            detail: "来自 EASY_DEPLOY_DATABASE_URL / --database-url",
        },
        SettingsRow {
            label: "制品存储",
            value: storage_provider_label(&platform_config.artifact_storage.provider).to_owned(),
            detail: "OpenAPI 版本包投递使用的平台级存储后端",
        },
        SettingsRow {
            label: "OSS Bucket",
            value: display_text(
                platform_config.artifact_storage.aliyun_oss.bucket.clone(),
                "未配置",
            ),
            detail: "阿里云 OSS 直传和目标节点下载使用的私有 Bucket",
        },
    ];
    let auth_rows = vec![
        SettingsRow {
            label: "会话存储",
            value: "内存".to_owned(),
            detail: "部署平台不依赖 Redis；服务重启会清空登录态，需要重新登录",
        },
        SettingsRow {
            label: "授权方案",
            value: "HttpOnly Cookie + Access/Refresh Token".to_owned(),
            detail: "Refresh Token 会轮换，会话可在后台强制下线",
        },
        SettingsRow {
            label: "Cookie Secure",
            value: if settings.cookie_secure {
                "已启用 Secure".to_owned()
            } else {
                "未启用 Secure".to_owned()
            },
            detail: "来自 EASY_DEPLOY_COOKIE_SECURE / --cookie-secure，生产 HTTPS 建议启用",
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
            detail: "内置 Compose 模板创建应用时的默认主机端口，可在创建表单里覆盖",
        },
        SettingsRow {
            label: "默认节点目录",
            value: platform_config.default_node_work_dir.clone(),
            detail: "新增节点时的默认工作目录，可在本页保存后立即用于新建表单",
        },
        SettingsRow {
            label: "健康检查超时",
            value: "5 秒".to_owned(),
            detail: "应用未配置时使用 none；HTTP/TCP/容器运行状态检查默认 5 秒",
        },
        SettingsRow {
            label: "命令执行超时",
            value: format!("{} 秒", settings.command_timeout_secs.max(1)),
            detail: "来自 EASY_DEPLOY_COMMAND_TIMEOUT_SECS / --command-timeout-secs，作用于 Docker、SSH 与 scp 命令",
        },
        SettingsRow {
            label: "发布版本保留",
            value: format!(
                "最多 {} 个版本",
                platform_config.uploaded_binary_releases_to_keep
            ),
            detail: "页面或 OpenAPI 投递版本包后的保留数量，当前生效版本永远不会被清理",
        },
        SettingsRow {
            label: "任务队列",
            value: "进程内 Tokio 队列".to_owned(),
            detail: "先保持单体易部署，后续再接外部 worker",
        },
    ];
    let nav_sections = nav_sections("/settings", &session);
    let oss = &platform_config.artifact_storage.aliyun_oss;
    let aliyun_oss_secret_status = if oss.access_key_secret.trim().is_empty() {
        "未配置"
    } else {
        "已配置"
    };
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
        artifact_storage_provider: &platform_config.artifact_storage.provider,
        aliyun_oss_region: &oss.region,
        aliyun_oss_endpoint: &oss.endpoint,
        aliyun_oss_bucket: &oss.bucket,
        aliyun_oss_object_prefix: &oss.object_prefix,
        aliyun_oss_access_key_id: &oss.access_key_id,
        aliyun_oss_secret_status,
        aliyun_oss_upload_url_ttl_seconds: oss.upload_url_ttl_seconds,
        aliyun_oss_download_url_ttl_seconds: oss.download_url_ttl_seconds,
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
                artifact_storage_provider: form.artifact_storage_provider,
                aliyun_oss_region: form.aliyun_oss_region,
                aliyun_oss_endpoint: form.aliyun_oss_endpoint,
                aliyun_oss_bucket: form.aliyun_oss_bucket,
                aliyun_oss_object_prefix: form.aliyun_oss_object_prefix,
                aliyun_oss_access_key_id: form.aliyun_oss_access_key_id,
                aliyun_oss_access_key_secret: form.aliyun_oss_access_key_secret,
                aliyun_oss_upload_url_ttl_seconds: form.aliyun_oss_upload_url_ttl_seconds,
                aliyun_oss_download_url_ttl_seconds: form.aliyun_oss_download_url_ttl_seconds,
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
                    "更新平台设置：应用目录模板 {}，节点目录 {}，发布版本保留 {} 个版本",
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

async fn events_page(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(query): Query<EventLogQuery>,
) -> Response {
    if !session.can(AUDIT_VIEW) {
        return forbidden();
    }
    let filter = EventLogFilter {
        event_type: query.event_type.clone(),
        level: query.level.clone(),
        target_type: query.target_type.clone(),
        query: query.q.clone(),
    };
    let logs = match state.events().list_filtered(filter).await {
        Ok(logs) => logs,
        Err(err) => return event_error_response(err),
    };
    let event_type_options = match state.events().event_type_options().await {
        Ok(options) => options,
        Err(err) => return event_error_response(err),
    };
    let target_options = match state.events().target_type_options().await {
        Ok(options) => options,
        Err(err) => return event_error_response(err),
    };
    let selected_event_type = query.event_type.as_deref().unwrap_or_default();
    let selected_level = query.level.as_deref().unwrap_or_default();
    let selected_target_type = query.target_type.as_deref().unwrap_or_default();
    let query_text = query.q.as_deref().unwrap_or_default();
    let rows = logs
        .iter()
        .map(|log| EventLogRow {
            id: log.id,
            event_type: &log.event_type,
            level: event_level_label(&log.level),
            level_tone: event_level_tone(&log.level),
            target: event_target_text(log),
            title: &log.title,
            summary: &log.summary,
            detail: &log.detail,
            created_at: &log.created_at,
            has_detail: !log.detail.trim().is_empty(),
        })
        .collect::<Vec<_>>();
    let event_type_filters = event_type_options
        .into_iter()
        .map(|option| AuditFilterOptionRow {
            selected: option.value == selected_event_type,
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
    let nav_sections = nav_sections("/events", &session);
    render_html(EventsTemplate {
        product_name: "Easy Deploy",
        css: include_str!("../../assets/app.css"),
        asset_version: ASSET_VERSION,
        release_version: concat!("v", env!("CARGO_PKG_VERSION")),
        current_user: session.display_name(),
        csrf_token: &session.csrf_token,
        nav_sections: &nav_sections,
        logs: &rows,
        event_type_filters: &event_type_filters,
        target_filters: &target_filters,
        selected_event_type,
        selected_level,
        selected_target_type,
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
    let created = match query.created.as_deref() {
        Some(nonce) => take_api_token_flash(&state, nonce).await,
        None => None,
    };
    render_api_tokens_page(&state, &session, created, query.notice.as_deref()).await
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
        Ok(created) => {
            let nonce = store_api_token_flash(&state, created).await;
            redirect(&format!("/admin/api-tokens?created={nonce}"))
        }
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

async fn api_token_delete_submit(
    State(state): State<AppState>,
    session: CurrentSession,
    Form(form): Form<ApiTokenDeleteForm>,
) -> Response {
    if !valid_csrf(&session, &form.csrf_token) {
        return forbidden();
    }
    if !session.can(API_TOKENS_MANAGE) {
        return forbidden();
    }
    match state
        .auth()
        .delete_revoked_api_token(&session, form.token_id)
        .await
    {
        Ok(()) => redirect("/admin/api-tokens?notice=deleted"),
        Err(err) => (err.status_code(), err.message().to_owned()).into_response(),
    }
}

async fn api_v1_upload_service_package(
    State(state): State<AppState>,
    api: ApiSession,
    Path(service_key): Path<String>,
    multipart: Multipart,
) -> Response {
    if !api.can(ARTIFACTS_UPLOAD) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let upload = match parse_api_package_upload_multipart(multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };
    let parsed = match parse_release_package_name_for_service(
        &upload.file_name,
        &service_key,
        Some(&upload.artifact_version),
    ) {
        Ok(parsed) => parsed,
        Err(err) => return api_package_error(StatusCode::BAD_REQUEST, err),
    };
    let app_id = match state.apps().app_id_by_key(&service_key).await {
        Ok(app_id) => app_id,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    let result = match state
        .apps()
        .upload_release_package(UploadReleasePackageInput {
            app_id,
            release_version: parsed.release_version,
            version_code: upload.version_code.or(Some(parsed.version_code)),
            published_at: upload.published_at,
            file_name: upload.file_name,
            bytes: upload.bytes,
            entry_file: upload.entry_file,
            source: upload.source,
        })
        .await
    {
        Ok(result) => result,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": {
            "app_id": result.app_id,
            "service_key": result.app_key,
            "release_id": result.release_id,
            "queue_id": result.queue_id,
            "release_version": result.release_version,
            "version_code": result.version_code,
            "versionCode": result.version_code,
            "published_at": result.published_at,
            "publishedAt": result.published_at,
            "package_path": result.package_path,
            "package_kind": result.package_kind,
            "config_snapshot_id": result.config_snapshot_id,
            "config_revision_no": result.config_revision_no,
            "config_revision": format!("config#{}", result.config_revision_no),
            "queued": result.queued,
            "task_id": serde_json::Value::Null
        }
    }))
    .into_response()
}

async fn api_v1_create_service_package_upload(
    State(state): State<AppState>,
    api: ApiSession,
    Path(service_key): Path<String>,
    Json(payload): Json<ApiCreatePackageUploadRequest>,
) -> Response {
    if !api.can(ARTIFACTS_UPLOAD) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let parsed = match parse_release_package_name_for_service(
        &payload.file_name,
        &service_key,
        Some(&payload.release_version),
    ) {
        Ok(parsed) => parsed,
        Err(err) => return api_package_error(StatusCode::BAD_REQUEST, err),
    };
    let app_id = match state.apps().app_id_by_key(&service_key).await {
        Ok(app_id) => app_id,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    let result = match state
        .apps()
        .create_release_package_upload(CreateReleasePackageUploadInput {
            app_id,
            release_version: parsed.release_version,
            version_code: payload.version_code.or(Some(parsed.version_code)),
            published_at: payload.published_at,
            file_name: payload.file_name,
            source: payload.source,
        })
        .await
    {
        Ok(result) => result,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    let upload_headers = result
        .upload_headers
        .iter()
        .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
        .collect::<serde_json::Map<_, _>>();
    Json(serde_json::json!({
        "data": {
            "app_id": result.app_id,
            "service_key": result.app_key,
            "upload_id": result.upload_id,
            "release_version": result.release_version,
            "version_code": result.version_code,
            "versionCode": result.version_code,
            "file_name": result.file_name,
            "object_key": result.object_key,
            "bucket": result.bucket,
            "endpoint": result.endpoint,
            "upload": {
                "method": result.upload_method,
                "url": result.upload_url,
                "headers": upload_headers,
                "expires_at": result.expires_at,
                "expiresAt": result.expires_at
            },
            "complete_path": result.complete_path
        }
    }))
    .into_response()
}

async fn api_v1_complete_service_package_upload(
    State(state): State<AppState>,
    api: ApiSession,
    Path((service_key, upload_id)): Path<(String, String)>,
    Json(payload): Json<ApiCompletePackageUploadRequest>,
) -> Response {
    if !api.can(ARTIFACTS_UPLOAD) {
        return api_error(StatusCode::FORBIDDEN, "permission denied");
    }
    let result = match state
        .apps()
        .complete_release_package_upload(CompleteReleasePackageUploadInput {
            upload_id,
            service_key,
            checksum_sha256: payload.checksum_sha256,
            size_bytes: payload.size_bytes,
            published_at: payload.published_at,
            source: payload.source,
        })
        .await
    {
        Ok(result) => result,
        Err(err) => return api_error(app_error_status(&err), err.message()),
    };
    Json(serde_json::json!({
        "data": {
            "app_id": result.app_id,
            "service_key": result.app_key,
            "release_id": result.release_id,
            "queue_id": result.queue_id,
            "release_version": result.release_version,
            "version_code": result.version_code,
            "versionCode": result.version_code,
            "published_at": result.published_at,
            "publishedAt": result.published_at,
            "package_path": result.package_path,
            "package_kind": result.package_kind,
            "config_snapshot_id": result.config_snapshot_id,
            "config_revision_no": result.config_revision_no,
            "config_revision": format!("config#{}", result.config_revision_no),
            "queued": result.queued,
            "task_id": serde_json::Value::Null
        }
    }))
    .into_response()
}

async fn api_v1_upload_unit_release(
    State(state): State<AppState>,
    api: ApiSession,
    Path((app_key, unit_key)): Path<(String, String)>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if !api.can(ARTIFACTS_UPLOAD) {
        return api_error_code(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "permission denied",
        );
    }
    let idempotency_key = match required_idempotency_key(&headers) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let (app_id, unit_id) = match sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT apps.id, units.id FROM apps
        JOIN deployment_units units ON units.app_id = apps.id
        WHERE apps.app_key = ?1 AND units.unit_key = ?2
        "#,
    )
    .bind(&app_key)
    .bind(&unit_key)
    .fetch_optional(state.db())
    .await
    {
        Ok(Some(ids)) => ids,
        Ok(None) => {
            return api_error_code(
                StatusCode::NOT_FOUND,
                "UNIT_NOT_FOUND",
                "application or deployment unit not found",
            );
        }
        Err(error) => {
            return api_error_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                "DATABASE_ERROR",
                &error.to_string(),
            );
        }
    };
    if !api.allows_app(app_id) || !api.allows_unit(unit_id) {
        return api_error_code(
            StatusCode::FORBIDDEN,
            "RESOURCE_SCOPE_DENIED",
            "API Token is not scoped to this application and deployment unit",
        );
    }
    let upload = match parse_api_package_upload_multipart(multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };
    if let Err(error) = validate_version(&upload.artifact_version) {
        return application_release_api_error(error);
    }
    if upload.bytes.is_empty() {
        return api_error_code(
            StatusCode::BAD_REQUEST,
            "EMPTY_PACKAGE",
            "unit release package cannot be empty",
        );
    }
    let checksum = format!("{:x}", Sha256::digest(&upload.bytes));
    let request_hash = stable_request_hash(&[
        app_key.as_bytes(),
        unit_key.as_bytes(),
        upload.artifact_version.as_bytes(),
        upload.file_name.as_bytes(),
        checksum.as_bytes(),
    ]);
    match idempotency_replay(
        state.db(),
        api.token_id(),
        "unit_release.upload",
        &idempotency_key,
        &request_hash,
    )
    .await
    {
        Ok(Some(response)) => return response,
        Ok(None) => {}
        Err(response) => return response,
    }
    let runtime_fs = RuntimeFs::new(state.settings().data_dir.clone());
    let staged = match runtime_fs
        .stage_unit_release_package_file(
            &app_key,
            &unit_key,
            &upload.artifact_version,
            &upload.file_name,
            &upload.bytes,
        )
        .await
    {
        Ok(staged) => staged,
        Err(error) => {
            return api_error_code(
                StatusCode::CONFLICT,
                "PACKAGE_STAGE_FAILED",
                &error.to_string(),
            );
        }
    };
    if let Err(error) = runtime_fs.promote_staged_release_package(&staged).await {
        let _ = runtime_fs.discard_staged_release_package(&staged).await;
        return api_error_code(
            StatusCode::CONFLICT,
            "UNIT_VERSION_EXISTS",
            &error.to_string(),
        );
    }
    let result = state
        .application_releases()
        .register_unit_release(RegisterUnitReleaseInput {
            unit_id,
            version: upload.artifact_version,
            package_name: upload.file_name,
            package_path: staged.package_path.to_string_lossy().into_owned(),
            extract_dir: staged.release_dir.to_string_lossy().into_owned(),
            checksum_sha256: checksum,
            size_bytes: upload.bytes.len() as i64,
            published_at: upload.published_at,
            source: "openapi".to_owned(),
            metadata: serde_json::json!({
                "source": upload.source,
                "entry_file": upload.entry_file,
                "idempotency_request_hash": request_hash,
            }),
            storage: UnitReleaseStorage::local(),
        })
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = runtime_fs.remove_promoted_release_package(&staged).await;
            return application_release_api_error(error);
        }
    };
    let body = serde_json::json!({
        "data": {
            "app_id": app_id,
            "app_key": app_key,
            "unit_id": unit_id,
            "unit_key": unit_key,
            "unit_release_id": result.release_id,
            "version": result.version,
            "version_code": result.version_code,
            "versionCode": result.version_code,
            "checksum_sha256": result.checksum_sha256,
            "deprecated": false
        }
    });
    if let Err(response) = store_idempotency_response(
        state.db(),
        api.token_id(),
        "unit_release.upload",
        &idempotency_key,
        &request_hash,
        "deployment_unit_release",
        &result.release_id.to_string(),
        StatusCode::CREATED,
        &body,
    )
    .await
    {
        return response;
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn api_v1_create_application_release(
    State(state): State<AppState>,
    api: ApiSession,
    Path(app_key): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<ApiCreateApplicationReleaseRequest>,
) -> Response {
    if !api.can(ARTIFACTS_UPLOAD) {
        return api_error_code(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "permission denied",
        );
    }
    let idempotency_key = match required_idempotency_key(&headers) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let app_id = match sqlx::query_scalar::<_, i64>("SELECT id FROM apps WHERE app_key = ?1")
        .bind(&app_key)
        .fetch_optional(state.db())
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            return api_error_code(
                StatusCode::NOT_FOUND,
                "APP_NOT_FOUND",
                "application not found",
            );
        }
        Err(error) => {
            return api_error_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                "DATABASE_ERROR",
                &error.to_string(),
            );
        }
    };
    if !api.allows_app(app_id)
        || payload
            .unit_changes
            .iter()
            .any(|change| !api.allows_unit(change.unit_id))
    {
        return api_error_code(
            StatusCode::FORBIDDEN,
            "RESOURCE_SCOPE_DENIED",
            "API Token is not scoped to this application and all changed deployment units",
        );
    }
    let payload_json = match serde_json::to_vec(&payload) {
        Ok(value) => value,
        Err(error) => {
            return api_error_code(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                &error.to_string(),
            );
        }
    };
    let request_hash = stable_request_hash(&[app_key.as_bytes(), &payload_json]);
    match idempotency_replay(
        state.db(),
        api.token_id(),
        "application_release.create",
        &idempotency_key,
        &request_hash,
    )
    .await
    {
        Ok(Some(response)) => return response,
        Ok(None) => {}
        Err(response) => return response,
    }
    let result = state
        .application_releases()
        .create_application_release(CreateApplicationReleaseInput {
            app_id,
            version: payload.version,
            base_app_release_id: payload.base_app_release_id,
            unit_changes: payload
                .unit_changes
                .into_iter()
                .map(|change| UnitReleaseChange {
                    unit_id: change.unit_id,
                    unit_release_id: change.unit_release_id,
                    desired_status: change.desired_status,
                })
                .collect(),
            environment_configs: payload
                .environment_configs
                .into_iter()
                .map(|selection| EnvironmentConfigSelection {
                    environment_id: selection.environment_id,
                    config_revision_id: selection.config_revision_id,
                })
                .collect(),
            created_by: format!("api-token:{}", api.token_id()),
        })
        .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => return application_release_api_error(error),
    };
    let body = serde_json::json!({
        "data": {
            "app_id": app_id,
            "app_key": app_key,
            "app_release_id": result.app_release_id,
            "version": result.version,
            "version_code": result.version_code,
            "versionCode": result.version_code,
            "manifest_hash": result.manifest_hash,
            "units": result.units,
            "environment_configs": result.environment_configs,
            "deployment_started": false
        }
    });
    if let Err(response) = store_idempotency_response(
        state.db(),
        api.token_id(),
        "application_release.create",
        &idempotency_key,
        &request_hash,
        "application_release",
        &result.app_release_id.to_string(),
        StatusCode::CREATED,
        &body,
    )
    .await
    {
        return response;
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn openapi_json() -> impl IntoResponse {
    Json(openapi_spec())
}

async fn openapi_docs() -> impl IntoResponse {
    Html(openapi_docs_public_html())
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
            can_delete: session.can(API_TOKENS_MANAGE) && token.status == "revoked",
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

async fn store_api_token_flash(state: &AppState, created: crate::auth::CreatedApiToken) -> String {
    let nonce = format!(
        "{}-{}",
        created.token_prefix,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    );
    let mut flashes = state.inner.api_token_flashes.lock().await;
    if flashes.len() >= 20
        && let Some(oldest_key) = flashes.keys().next().cloned()
    {
        flashes.remove(&oldest_key);
    }
    flashes.insert(nonce.clone(), created);
    nonce
}

async fn take_api_token_flash(
    state: &AppState,
    nonce: &str,
) -> Option<crate::auth::CreatedApiToken> {
    if nonce.trim().is_empty() {
        return None;
    }
    state.inner.api_token_flashes.lock().await.remove(nonce)
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

fn node_check_result_node_status_tone(status: &str) -> &'static str {
    if status == "passed" {
        node_status_tone("online")
    } else {
        node_status_tone("offline")
    }
}

fn node_check_result_node_status_label_clean(status: &str) -> &'static str {
    if status == "passed" {
        node_status_label_clean("online")
    } else {
        node_status_label_clean("offline")
    }
}

fn node_status_label_clean(status: &str) -> &'static str {
    match status {
        "online" => "在线",
        "offline" => "离线",
        "disabled" => "已禁用",
        _ => "未探测",
    }
}

fn node_page_row_clean<'a>(
    node: &'a crate::nodes::NodeListItem,
    can_manage: bool,
) -> NodePageRow<'a> {
    NodePageRow {
        id: node.id,
        name: &node.name,
        node_key: &node.node_key,
        node_type: node_type_label_clean(&node.node_type),
        address: &node.address,
        ssh: if node.node_type == "ssh" {
            format!("{}:{}", node.ssh_user, node.ssh_port)
        } else {
            "本地执行".to_owned()
        },
        ssh_port: node.ssh_port,
        ssh_user: &node.ssh_user,
        credential_id: node.credential_id.unwrap_or_default(),
        credential_name: node_credential_display_name_clean(node),
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
        status: node_status_label_clean(&node.status),
        status_tone: node_status_tone(&node.status),
        docker_status: &node.docker_status,
        capability: node_capability_text_clean(node),
        os_info: node_probe_detail_text_clean(node.last_os_info.as_deref(), "OS 未探测"),
        disk_info: node_disk_detail_text_clean(node.last_disk_info.as_deref(), "磁盘未探测"),
        systemd_version: node_probe_detail_text_clean(
            node.last_systemd_version.as_deref(),
            "systemd 未探测",
        ),
        proxy_version: node_proxy_version_text_clean(node),
        last_check_at: node.last_check_at.as_deref().unwrap_or("尚未探测"),
        last_message: node.last_message.as_deref().unwrap_or("等待节点探测"),
        can_manage,
        is_ssh: node.node_type == "ssh",
        can_check: node.status != "disabled",
        toggle_status: node_status_toggle_value(&node.status),
        toggle_label: node_status_toggle_label_clean(&node.status),
    }
}

fn node_check_history_row_clean(check: &crate::nodes::NodeCheckHistoryItem) -> NodeCheckHistoryRow {
    NodeCheckHistoryRow {
        id: check.id,
        status: node_check_status_label_clean(&check.check_status),
        status_tone: node_check_status_tone(&check.check_status),
        message: display_text_clean(check.message.clone(), "未记录"),
        docker_version: display_text_clean(check.docker_version.clone(), "未记录"),
        compose_version: display_text_clean(check.compose_version.clone(), "未记录"),
        os_info: node_probe_detail_text_clean(Some(&check.os_info), "OS 未探测"),
        disk_info: node_disk_detail_text_clean(Some(&check.disk_info), "磁盘未探测"),
        systemd_version: node_probe_detail_text_clean(
            Some(&check.systemd_version),
            "systemd 未探测",
        ),
        checked_at: check.checked_at.clone(),
    }
}

fn node_app_runtime_row_clean(app: &crate::nodes::NodeAppRuntimeItem) -> NodeAppRuntimeRow {
    NodeAppRuntimeRow {
        app_id: app.app_id,
        app_name: app.app_name.clone(),
        app_key: app.app_key.clone(),
        app_type: app_type_label(&app.app_type),
        app_status: app_enabled_status_label(&app.app_status),
        app_status_tone: app_enabled_status_tone(&app.app_status),
        runtime_status: runtime_status_label(&app.runtime_status),
        runtime_status_tone: runtime_status_tone(&app.runtime_status),
        active_version: display_text_clean(app.active_version.clone(), "未部署"),
        service_count: app.service_count,
        message: display_text_clean(app.message.clone(), "暂无运行信息"),
        last_deploy_at: app
            .last_deploy_at
            .clone()
            .unwrap_or_else(|| "未部署".to_owned()),
        updated_at: app.updated_at.clone(),
    }
}

fn node_task_row_clean(task: &crate::nodes::NodeTaskItem) -> NodeTaskRow {
    NodeTaskRow {
        id: task.id,
        title: crate::text::fix_mojibake(&task.title),
        task_kind: task_kind_label(&task.task_kind),
        app_name: display_text_clean(task.app_name.clone(), "未关联应用"),
        status: task_status_label(&task.status),
        status_tone: task_status_tone(&task.status),
        phase: task_phase_label(&task.phase),
        summary: display_text_clean(task.summary.clone(), "暂无摘要"),
        created_by: task.created_by.clone(),
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
    }
}

fn node_type_label_clean(node_type: &str) -> &'static str {
    match node_type {
        "local" => "本机",
        "ssh" => "SSH",
        _ => "未知类型",
    }
}

fn node_status_toggle_label_clean(status: &str) -> &'static str {
    if status == "disabled" {
        "启用"
    } else {
        "禁用"
    }
}

fn node_credential_display_name_clean(node: &crate::nodes::NodeListItem) -> String {
    if node.node_type != "ssh" {
        return "本地执行".to_owned();
    }
    node.credential_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(crate::text::fix_mojibake)
        .unwrap_or_else(|| "系统 SSH 配置".to_owned())
}

fn node_capability_text_clean(node: &crate::nodes::NodeListItem) -> String {
    let executor = if node.node_type == "local" {
        "本地执行"
    } else {
        "SSH 执行"
    };
    let docker = node
        .last_docker_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(crate::text::fix_mojibake)
        .unwrap_or_else(|| crate::text::fix_mojibake(node.docker_status.as_str()));
    let compose = node
        .last_compose_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(crate::text::fix_mojibake)
        .unwrap_or_else(|| "Compose 未探测".to_owned());
    let proxy = node_proxy_capability_text_clean(node);
    format!("{executor} / {docker} / {compose} / {proxy}")
}

fn node_proxy_capability_text_clean(node: &crate::nodes::NodeListItem) -> String {
    match (node.caddy_available == 1, node.nginx_available == 1) {
        (true, true) => "Caddy/Nginx 可用".to_owned(),
        (true, false) => "Caddy 可用".to_owned(),
        (false, true) => "Nginx 可用".to_owned(),
        (false, false) => "代理未探测".to_owned(),
    }
}

fn node_proxy_version_text_clean(node: &crate::nodes::NodeListItem) -> String {
    let mut versions = Vec::new();
    if let Some(caddy) = node
        .last_caddy_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        versions.push(crate::text::fix_mojibake(&format!(
            "Caddy {}",
            first_line(caddy)
        )));
    }
    if let Some(nginx) = node
        .last_nginx_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        versions.push(crate::text::fix_mojibake(first_line(nginx)));
    }
    if versions.is_empty() {
        "代理未探测".to_owned()
    } else {
        versions.join(" / ")
    }
}

fn node_probe_detail_text_clean(value: Option<&str>, fallback: &'static str) -> String {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        crate::text::fix_mojibake(value.lines().next().unwrap_or(value).trim())
    }
}

fn node_disk_detail_text_clean(value: Option<&str>, fallback: &'static str) -> String {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        return fallback.to_owned();
    }
    crate::text::fix_mojibake(
        value
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("Filesystem"))
            .unwrap_or(value),
    )
}

fn node_check_status_label_clean(status: &str) -> &'static str {
    match status {
        "passed" => "通过",
        "failed" => "失败",
        _ => "未知",
    }
}

fn display_text_clean(value: String, fallback: &'static str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        crate::text::fix_mojibake(&value)
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
        "rolling_continue" => "某个节点失败后继续执行后续节点，任务最终汇总为失败",
        _ => "某个节点失败后停止后续节点执行，未执行节点会标记为跳过",
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
            "创建 {} 任务，按目标节点顺序滚动执行",
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
    rows.push(DeployPlanStepRow {
        label: deploy_plan_execute_label(action),
        detail: deploy_plan_execute_detail(detail, action),
        tone: deploy_confirm_action_tone(action),
    });
    if deploy_confirm_runs_health_check(action) {
        rows.push(DeployPlanStepRow {
            label: "健康检查",
            detail: format!(
                "执行 {}，超时 {} 秒；失败时当前节点会标记为异常",
                detail.health_check.kind.label(),
                detail.health_check.timeout_secs
            ),
            tone: "success",
        });
    }
    rows.push(DeployPlanStepRow {
        label: "结果回写",
        detail: "记录任务日志、节点结果、部署历史和应用运行状态".to_owned(),
        tone: "neutral",
    });
    rows
}

fn deploy_plan_files(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> Vec<DeployPlanFileRow> {
    let DeployConfirmAction::Compose(_) = action;
    vec![
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
    ]
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
            "当前没有已知阻断项；提交后仍会执行任务级预检".to_owned(),
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
            "{} 个目标节点存在已知阻断项：{}。请先完成节点探测、安装缺失组件或调整目标节点后再提交",
            blocked_nodes.len(),
            blocked_nodes.join("、")
        ))
    }
}

fn deploy_preflight_checks(
    _detail: &crate::apps::AppConfigDetail,
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

    let DeployConfirmAction::Compose(_) = action;
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

    checks
}

fn deploy_preflight_actions(
    _detail: &crate::apps::AppConfigDetail,
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
    let DeployConfirmAction::Compose(_) = action;
    if node.docker_available == 0 {
        actions.push(deploy_install_action("安装 Docker", "docker"));
    }
    if node.compose_available == 0 {
        actions.push(deploy_install_action("安装 Compose", "compose"));
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
    let DeployConfirmAction::Compose(_) = action;
    "校验节点在线状态、Docker daemon、docker compose config、部署目录和磁盘空间".to_owned()
}

fn deploy_plan_sync_detail(
    detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> String {
    let DeployConfirmAction::Compose(_) = action;
    format!(
        "compose.yaml、.env 与 app.yaml 同步到 {}",
        detail.app.work_dir
    )
}

fn deploy_plan_execute_label(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => "执行部署",
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => "执行停止",
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => "执行重启",
    }
}

fn deploy_plan_execute_detail(
    _detail: &crate::apps::AppConfigDetail,
    action: DeployConfirmAction,
) -> String {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => {
            "运行 docker compose up -d --remove-orphans".to_owned()
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => {
            "运行 docker compose down".to_owned()
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => {
            "运行 docker compose restart".to_owned()
        }
    }
}

fn deploy_confirm_syncs_files(action: DeployConfirmAction) -> bool {
    let DeployConfirmAction::Compose(_) = action;
    true
}

fn deploy_confirm_runs_health_check(action: DeployConfirmAction) -> bool {
    matches!(
        action,
        DeployConfirmAction::Compose(ComposeTaskAction::Up)
            | DeployConfirmAction::Compose(ComposeTaskAction::Restart)
    )
}

fn node_status_toggle_value(status: &str) -> &'static str {
    if status == "disabled" {
        "unknown"
    } else {
        "disabled"
    }
}

fn node_credential_fingerprint(node: &crate::nodes::NodeListItem) -> String {
    node.credential_fingerprint
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("")
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
                "节点还没有通过 Docker CLI 与 daemon 检查，Compose 应用无法部署",
            ),
            command: format!(
                "{install_prefix}curl -fsSL https://get.docker.com | sudo sh && sudo systemctl enable --now docker"
            ),
            verify: "重新探测后应看到 Docker 版本与 online 状态",
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
                "Docker 可用，但 docker compose version 未通过，Compose 应用无法执行",
            ),
            command: format!(
                "{install_prefix}sudo apt-get update && sudo apt-get install -y docker-compose-plugin"
            ),
            verify: "重新探测后应看到 Docker Compose version v2.x",
            install_component: "compose",
            can_install: true,
        });
    }

    if guides.is_empty() {
        guides.push(NodeCapabilityGuideRow {
            title: "节点能力已就绪",
            tone: "success",
            reason: "Docker 与 Compose 最近一次探测均可用".to_owned(),
            command: if node.node_type == "ssh" {
                format!("{install_prefix}docker compose version")
            } else {
                "docker compose version".to_owned()
            },
            verify: "可以继续作为 Compose 部署目标使用",
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
        "binary" => "历史兼容",
        _ => "Docker Compose",
    }
}

fn app_environment_label(environment: &str) -> &'static str {
    match environment {
        "production" => "正式环境",
        _ => "测试环境",
    }
}

fn app_environment_tone(environment: &str) -> &'static str {
    match environment {
        "production" => "active",
        _ => "neutral",
    }
}

fn deploy_strategy_label(strategy: &str) -> &'static str {
    match strategy {
        "rolling_continue" => "逐节点继续，最终汇总失败",
        _ => "滚动部署，失败停止",
    }
}

fn release_source_label(source: &str) -> &'static str {
    match source {
        "manual" => "手动配置发布",
        _ => "版本包上传",
    }
}

fn artifact_notice_message(notice: Option<&str>) -> &'static str {
    match notice {
        Some("app-deploying") => APP_DEPLOYMENT_IN_PROGRESS_MESSAGE,
        _ => "",
    }
}

fn app_enabled_status_label(status: &str) -> &'static str {
    match status {
        "disabled" => "已停用",
        _ => "已启用",
    }
}

fn app_enabled_status_tone(status: &str) -> &'static str {
    match status {
        "disabled" => "warning",
        _ => "success",
    }
}

fn app_runtime_status_label(status: &str) -> &'static str {
    match status {
        "healthy" => "健康",
        "unhealthy" => "异常",
        "deploying" => "部署中",
        "stopped" => "已停止",
        "disabled" => "已停用",
        _ => "未部署",
    }
}

fn app_runtime_status_tone(status: &str) -> &'static str {
    match status {
        "healthy" => "success",
        "deploying" => "active",
        "unhealthy" | "disabled" => "warning",
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
            label: "角色数",
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
            label: "权限数",
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
            detail: "最多 100 条后台登录会话".to_owned(),
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

fn compose_action_segment(action: ComposeTaskAction) -> &'static str {
    match action {
        ComposeTaskAction::Up => "up",
        ComposeTaskAction::Down => "down",
        ComposeTaskAction::Restart => "restart",
    }
}

fn compose_submit_path(app_id: i64, action: ComposeTaskAction) -> String {
    format!("/apps/{app_id}/compose/{}", compose_action_segment(action))
}

fn compose_confirm_path(app_id: i64, action: ComposeTaskAction) -> String {
    format!("{}/confirm", compose_submit_path(app_id, action))
}

fn deploy_confirm_action_label(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => "部署",
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => "停止",
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => "重启",
    }
}

fn deploy_confirm_action_tone(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up)
        | DeployConfirmAction::Compose(ComposeTaskAction::Restart) => "active",
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => "warning",
    }
}

fn deploy_confirm_action_description(action: DeployConfirmAction) -> &'static str {
    match action {
        DeployConfirmAction::Compose(ComposeTaskAction::Up) => {
            "确认目标节点、配置差异和健康检查后，提交 Docker Compose 部署任务"
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Down) => {
            "确认目标节点后，提交 Docker Compose 停止任务"
        }
        DeployConfirmAction::Compose(ComposeTaskAction::Restart) => {
            "确认目标节点、配置差异和健康检查后，提交 Docker Compose 重启任务"
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

fn environment_status_label(status: &str) -> &'static str {
    match status {
        "configuring" => "配置中",
        "ready" => "可部署",
        "disabled" => "已停用",
        _ => "未知",
    }
}

fn deployment_action_label(action: DeploymentAction) -> &'static str {
    match action {
        DeploymentAction::Deploy => "部署",
        DeploymentAction::Skip => "跳过",
        DeploymentAction::Start => "启动",
        DeploymentAction::Stop => "停止",
        DeploymentAction::Upgrade => "升级",
        DeploymentAction::Downgrade => "降级",
        DeploymentAction::Restore => "恢复结构",
        DeploymentAction::ApplicationCheck => "应用检查",
    }
}

fn deployment_action_tone(action: DeploymentAction) -> &'static str {
    match action {
        DeploymentAction::Skip => "neutral",
        DeploymentAction::Stop | DeploymentAction::Downgrade | DeploymentAction::Restore => {
            "warning"
        }
        _ => "active",
    }
}

fn console_deployment_status_label(status: &str) -> &'static str {
    match status {
        "waiting" => "等待部署",
        "queued" => "排队中",
        "running" => "部署中",
        "reconciling" => "待人工核对",
        "success" => "成功",
        "partial_failed" => "部分失败",
        "all_failed" | "failed" => "全部失败",
        "canceled" => "已取消",
        _ => "未知",
    }
}

fn deployment_orchestrator_error_response(
    error: crate::deployment_orchestrator::DeploymentOrchestratorError,
) -> Response {
    use crate::deployment_orchestrator::DeploymentOrchestratorError;
    let status = match &error {
        DeploymentOrchestratorError::Validation(_) => StatusCode::BAD_REQUEST,
        DeploymentOrchestratorError::Conflict(_) => StatusCode::CONFLICT,
        DeploymentOrchestratorError::NotFound(_) => StatusCode::NOT_FOUND,
        DeploymentOrchestratorError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string()).into_response()
}

fn console_deployment_status_tone(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "queued" | "running" => "active",
        "reconciling" | "partial_failed" | "all_failed" | "failed" => "warning",
        _ => "neutral",
    }
}

fn environment_status_tone(status: &str) -> &'static str {
    match status {
        "ready" => "success",
        "configuring" => "warning",
        _ => "neutral",
    }
}

fn environment_runtime_status_label(status: &str) -> &'static str {
    match status {
        "running" => "运行中",
        "partial_unhealthy" => "部分异常",
        "stopped" => "已停止",
        _ => "未部署",
    }
}

fn environment_runtime_status_tone(status: &str) -> &'static str {
    match status {
        "running" => "success",
        "partial_unhealthy" => "warning",
        _ => "neutral",
    }
}

fn unit_runtime_summary(
    unit: &crate::deployment_console::DeploymentUnitSummary,
) -> (String, &'static str) {
    if unit.deploying_count > 0 {
        return (format!("{} 个节点部署中", unit.deploying_count), "active");
    }
    if unit.unhealthy_count > 0 {
        return (format!("{} 个节点异常", unit.unhealthy_count), "warning");
    }
    if unit.node_count > 0 && unit.healthy_count == unit.node_count {
        return (format!("{} 个节点健康", unit.healthy_count), "success");
    }
    if unit.node_count > 0 && unit.stopped_count == unit.node_count {
        return (format!("{} 个节点已停止", unit.stopped_count), "neutral");
    }
    ("尚无可信运行状态".to_owned(), "neutral")
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
            status: if row.changed { "有变更" } else { "一致" },
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
            empty_title: "配置未变更",
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
            empty_title: "配置未变更",
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
        .unwrap_or("-")
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
    task_status == "failed" && is_compose_task_kind(task_kind)
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

fn app_page_href(
    app_type: &str,
    environment: &str,
    status: &str,
    query: &str,
    page: usize,
) -> String {
    let mut params = Vec::new();
    if !app_type.is_empty() {
        params.push(format!("type={}", encode_query_value(app_type)));
    }
    if !environment.is_empty() {
        params.push(format!("environment={}", encode_query_value(environment)));
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
        "未记录运行项".to_owned()
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

fn artifact_kind_label(kind: &str) -> &'static str {
    match kind {
        "tar_gz" => "tar.gz",
        "binary" => "单文件",
        _ => "unknown",
    }
}

fn artifact_source_label(source: &str) -> &'static str {
    match source {
        "upload" => "上传",
        "web" => "页面上传",
        "initial" => "初始登记",
        "openapi" => "OpenAPI",
        _ => "未知",
    }
}

fn storage_provider_label(provider: &str) -> &'static str {
    match provider {
        "aliyun_oss" => "阿里云 OSS",
        _ => "本机存储",
    }
}

fn release_status_label(status: &str) -> &'static str {
    match status {
        "received" => "待处理",
        "queued" => "等待入队",
        "deploying" => "发布中",
        "deployed" => "已发布",
        "failed" => "失败",
        "rolled_back" => "已回退",
        "canceled" => "已取消",
        _ => "未知",
    }
}

fn release_status_tone(status: &str) -> &'static str {
    match status {
        "deployed" => "success",
        "queued" | "deploying" => "active",
        "failed" => "danger",
        "canceled" | "rolled_back" => "warning",
        _ => "neutral",
    }
}

fn queue_status_label(status: &str) -> &'static str {
    match status {
        "scheduled" => "定时等待",
        "queued" => "等待中",
        "running" => "执行中",
        "success" => "已完成",
        "failed" => "失败",
        "canceled" => "已取消",
        _ => "未知",
    }
}

fn queue_status_tone(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "scheduled" | "queued" | "running" => "active",
        "failed" => "danger",
        "canceled" => "warning",
        _ => "neutral",
    }
}

fn normalize_artifact_status_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "received" => "received",
        "queued" => "queued",
        "deploying" => "deploying",
        "deployed" => "deployed",
        "failed" => "failed",
        "canceled" => "canceled",
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
        "web" => "web",
        "openapi" => "openapi",
        "initial" => "initial",
        _ => "",
    }
}

fn format_datetime_shanghai(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) else {
        return trimmed.to_owned();
    };
    let offset = FixedOffset::east_opt(8 * 60 * 60).expect("valid shanghai offset");
    parsed
        .with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn format_datetime_local_input(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) else {
        return String::new();
    };
    let offset = FixedOffset::east_opt(8 * 60 * 60).expect("valid shanghai offset");
    parsed
        .with_timezone(&offset)
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
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

fn count_apps_by_runtime(apps: &[crate::apps::AppListItem], status: &str) -> usize {
    apps.iter()
        .filter(|app| app.runtime_status == status)
        .count()
}

fn count_disabled_apps(apps: &[crate::apps::AppListItem]) -> usize {
    apps.iter().filter(|app| app.status == "disabled").count()
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

fn event_error_response(err: EventLogError) -> Response {
    let status = event_error_status(&err);
    (status, err.message().to_owned()).into_response()
}

fn event_error_status(err: &EventLogError) -> StatusCode {
    match err {
        EventLogError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        EventLogError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
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

fn task_step_status_label(status: &str) -> &'static str {
    match status {
        "pending" => "等待中",
        "running" => "执行中",
        "success" => "成功",
        "failed" => "失败",
        "skipped" => "已跳过",
        _ => "未知",
    }
}

fn task_step_status_tone(status: &str) -> &'static str {
    match status {
        "success" => "success",
        "running" => "active",
        "failed" => "warning",
        _ => "neutral",
    }
}

fn event_level_label(level: &str) -> &'static str {
    match level {
        "debug" => "调试",
        "info" => "信息",
        "warning" => "警告",
        "error" => "错误",
        _ => "未知",
    }
}

fn event_level_tone(level: &str) -> &'static str {
    match level {
        "error" | "warning" => "warning",
        "info" => "success",
        "debug" => "neutral",
        _ => "neutral",
    }
}

fn event_target_text(log: &crate::events::EventLogItem) -> String {
    let mut target = String::new();
    if !log.target_name.trim().is_empty() {
        target.push_str(&log.target_name);
    }
    if !log.target_type.trim().is_empty() || !log.target_id.trim().is_empty() {
        if !target.is_empty() {
            target.push_str(" · ");
        }
        target.push_str(&log.target_type);
        if !log.target_id.trim().is_empty() {
            target.push('#');
            target.push_str(&log.target_id);
        }
    }
    if target.is_empty() {
        "系统".to_owned()
    } else {
        target
    }
}

fn task_phase_label(phase: &str) -> &'static str {
    match phase {
        "queued" => "等待入队",
        "preflight" => "部署前预检",
        "preparing_files" => "准备运行文件",
        "executing" => "执行命令",
        "healthchecking" => "健康检查",
        "prepare" => "准备发布",
        "render" => "渲染配置",
        "pre_deploy" => "发布前脚本",
        "deploy" => "部署脚本",
        "post_deploy" => "发布后脚本",
        "switch_traffic" => "切换流量",
        "cleanup" => "清理现场",
        "finalize" => "收尾确认",
        "completed" => "已完成",
        "failed" => "失败收尾",
        "canceled" => "已取消",
        _ => "未知阶段",
    }
}

fn task_phase_tone(phase: &str) -> &'static str {
    match phase {
        "completed" => "success",
        "preflight" | "preparing_files" | "executing" | "healthchecking" | "prepare" | "render"
        | "pre_deploy" | "deploy" | "post_deploy" | "switch_traffic" | "cleanup" | "finalize" => {
            "active"
        }
        "failed" | "canceled" => "warning",
        _ => "neutral",
    }
}

fn task_phase_detail(phase: &str) -> &'static str {
    match phase {
        "queued" => "任务已创建，正在等待后台队列调度",
        "preflight" => "正在检查节点状态、Docker/Compose 能力、目录权限和端口风险",
        "preparing_files" => "正在准备 compose、环境变量、systemd unit、版本包或代理配置等运行文件",
        "executing" => "正在目标节点执行部署、停止、重启、安装或切流命令",
        "healthchecking" => "命令已执行完成，正在验证服务是否按预期运行",
        "prepare" => "正在准备发布上下文、版本包和配置快照",
        "render" => "正在渲染 Compose、环境变量和部署脚本",
        "pre_deploy" => "正在执行用户配置的发布前脚本",
        "deploy" => "正在执行部署脚本或 Docker Compose 命令",
        "post_deploy" => "正在执行发布后脚本",
        "switch_traffic" => "正在执行切流脚本",
        "cleanup" => "正在执行清理脚本",
        "finalize" => "正在完成发布收尾和状态写回",
        "completed" => "任务已经完成，节点结果和部署记录已写回",
        "failed" => "任务失败并完成收尾，请查看日志和节点结果定位原因",
        "canceled" => "任务在开始执行前已取消，不会再进入后台执行",
        _ => "当前阶段无法识别，请查看任务日志确认执行状态",
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
        "节点结果尚未写入，任务进入目标节点执行后会在这里汇总".to_owned()
    } else {
        format!(
            "{} 个节点已记录：{} 成功，{} 失败，{} 跳过",
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
        "queued" => "后台 worker 尚未开始执行，仍可取消该任务".to_owned(),
        "running" => task_phase_detail(&task.phase).to_owned(),
        "success" => {
            if failed_count == 0 && skipped_count == 0 {
                task_phase_detail(&task.phase).to_owned()
            } else {
                "任务已结束，请确认节点结果是否符合预期".to_owned()
            }
        }
        "failed" => {
            if failed_count > 0 {
                "至少一个目标节点失败；优先查看失败节点结果，再查看下方任务日志中的第一条错误"
                    .to_owned()
            } else {
                "任务在节点结果写入前失败；优先查看任务日志里的预检、命令或系统错误".to_owned()
            }
        }
        "canceled" => "任务在执行前被取消，没有继续下发到目标节点".to_owned(),
        _ => "当前状态无法识别，请查看任务日志确认实际执行情况".to_owned(),
    };
    let log_hint = match task.status.as_str() {
        "failed" => "先看失败摘要，再按时间顺序定位第一条错误日志",
        "running" => "页面会自动刷新，日志会按执行顺序继续追加",
        "queued" => "任务开始前通常只有入队日志",
        _ => "日志保留预检、命令输出和收尾信息",
    };
    let next_step = match task.status.as_str() {
        "queued" => "如果不想继续执行，可以点击右上角取消".to_owned(),
        "running" => "等待当前阶段完成；如果卡住，查看最新日志和目标节点连接状态".to_owned(),
        "success" => "可以返回应用详情查看运行状态、运行项日志和部署历史".to_owned(),
        "failed" if is_retryable_task_kind(&task.task_kind) => {
            "修复配置、节点能力或运行环境后，可以在右上角重试该任务".to_owned()
        }
        "failed" => "修复失败原因后，从对应页面重新发起操作".to_owned(),
        "canceled" => "需要执行时，请回到应用或节点页面重新发起操作".to_owned(),
        _ => "继续查看日志和元信息确认任务状态".to_owned(),
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
        "当前没有等待或执行中的部署任务".to_owned()
    } else {
        format!("{running} 个执行中，{queued} 个等待中")
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
            hint: "安装 Docker Compose 插件后重新探测节点，再回到任务或部署确认页",
        };
    }
    if message_mentions_component_issue(&message, &["docker daemon", "docker engine", "docker"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Docker",
            component: "docker",
            hint: "安装 Docker Engine 后重新探测节点，再重试部署任务",
        };
    }
    if message_mentions_component_issue(&message, &["caddy"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Caddy",
            component: "caddy",
            hint: "安装 Caddy 后重新探测节点，适用于 Blue/Green 反向代理切流",
        };
    }
    if message_mentions_component_issue(&message, &["nginx"]) {
        return TaskNodeResultAction {
            kind: "install",
            label: "安装 Nginx",
            component: "nginx",
            hint: "安装 Nginx 后重新探测节点，适用于 Nginx 反向代理切流",
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
            hint: "重新探测会刷新节点在线状态和组件能力",
        };
    }
    TaskNodeResultAction {
        kind: "detail",
        label: "查看节点",
        component: "",
        hint: "查看节点最近探测结果、组件能力和关联任务",
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

fn normalize_app_environment_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "production" => "production",
        "test" => "test",
        _ => "",
    }
}

fn normalize_app_runtime_status_filter(value: Option<&str>) -> &'static str {
    match value.unwrap_or_default() {
        "healthy" => "healthy",
        "unhealthy" => "unhealthy",
        "deploying" => "deploying",
        "stopped" => "stopped",
        "unknown" | "ready" | "draft" => "unknown",
        "disabled" => "disabled",
        _ => "",
    }
}

fn app_matches_filters(
    app: &crate::apps::AppListItem,
    selected_type: &str,
    selected_environment: &str,
    selected_status: &str,
    query: &str,
) -> bool {
    if !selected_type.is_empty() && app.app_type != selected_type {
        return false;
    }
    if !selected_environment.is_empty() && app.environment != selected_environment {
        return false;
    }
    if !selected_status.is_empty()
        && if selected_status == "disabled" {
            app.status != "disabled"
        } else {
            app.runtime_status != selected_status || app.status == "disabled"
        }
    {
        return false;
    }
    if query.trim().is_empty() {
        return true;
    }
    app_search_text(app).contains(&query.to_ascii_lowercase())
}

fn app_search_text(app: &crate::apps::AppListItem) -> String {
    format!(
        "{} {} {} {} {} {} {} {}",
        app.name,
        app.app_key,
        app.description,
        app_environment_label(&app.environment),
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
        "pre_deploy",
        "deploy",
        "post_deploy",
        "healthchecking",
        "switch_traffic",
        "cleanup",
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
        ("pre_deploy", "发布前脚本"),
        ("deploy", "部署脚本"),
        ("post_deploy", "发布后脚本"),
        ("healthchecking", "健康检查"),
        ("switch_traffic", "切换流量"),
        ("cleanup", "清理现场"),
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
    is_compose_task_kind(task_kind)
}

fn is_compose_task_kind(task_kind: &str) -> bool {
    matches!(task_kind, "compose.up" | "compose.down" | "compose.restart")
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

#[allow(dead_code)]
fn legacy_openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Easy Deploy Package Upload API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Easy Deploy 对外只暴露版本包投递能力。应用创建、部署配置、目标节点、发布策略、立即/定时发布都在后台页面维护，外部业务项目或 AI 只负责把规范命名的版本包上传到对应服务。"
        },
        "servers": [
            { "url": "http://127.0.0.1:9066", "description": "local default endpoint" }
        ],
        "security": [{ "BearerAuth": [] }],
        "paths": {
            "/api/v1/services/{service_key}/packages": {
                "post": {
                    "operationId": "uploadServicePackage",
                    "description": "向已在后台配置好的服务投递版本包。包名必须符合 {service_key}_version_{x_y_z}.tar.gz 或等价版本格式；平台接收后记录 release，并按应用的发布设置决定立即入队、定时发布或等待手动发布。",
                    "parameters": [
                        {
                            "name": "service_key",
                            "in": "path",
                            "required": true,
                            "description": "后台应用的服务标识，必须和版本包文件名前缀一致。",
                            "schema": { "type": "string" }
                        }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "multipart/form-data": {
                                "schema": { "$ref": "#/components/schemas/UploadServicePackageRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "uploaded",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/UploadServicePackageResponse" }
                                }
                            }
                        },
                        "400": {
                            "description": "package validation error",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/PackageError" }
                                }
                            }
                        },
                        "401": { "description": "missing or invalid token" },
                        "403": { "description": "token has no artifact upload permission" },
                        "404": { "description": "service_key does not match an existing app" }
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
                "UploadServicePackageRequest": {
                    "type": "object",
                    "required": ["package_file"],
                    "properties": {
                        "package_file": { "type": "string", "format": "binary", "description": "版本包文件。兼容字段名 file、artifact_file。" },
                        "release_version": { "type": "string", "description": "可选。显式版本号；如果填写，必须与包名解析出的版本一致。兼容字段名 artifact_version。" },
                        "version_code": { "type": "integer", "description": "可选。版本排序号；不填时平台从包名版本解析。兼容字段名 versionCode。" },
                        "published_at": { "type": "string", "description": "可选。业务项目构建或发布时间，ISO-8601 格式。兼容字段名 publishedAt。" },
                        "entry_file": { "type": "string", "description": "可选。保留给旧版本包结构的入口文件提示。Compose 发布主线通常不需要。" },
                        "source": { "type": "string", "description": "可选。来源标记，例如 ci、local-script、ai-agent。" }
                    }
                },
                "UploadServicePackageResponse": {
                    "type": "object",
                    "properties": {
                        "data": {
                            "type": "object",
                            "properties": {
                                "app_id": { "type": "integer" },
                                "service_key": { "type": "string" },
                                "release_id": { "type": "integer" },
                                "queue_id": { "type": ["integer", "null"], "description": "自动入队时返回队列 ID；未入队时为空。" },
                                "release_version": { "type": "string" },
                                "version_code": { "type": "integer" },
                                "versionCode": { "type": "integer" },
                                "published_at": { "type": "string" },
                                "publishedAt": { "type": "string" },
                                "package_path": { "type": "string" },
                                "package_kind": { "type": "string" },
                                "config_snapshot_id": { "type": "integer" },
                                "config_revision_no": { "type": "integer" },
                                "config_revision": { "type": "string" },
                                "queued": { "type": "boolean" },
                                "task_id": { "type": ["integer", "null"], "description": "上传接口只负责投递版本包，不直接执行部署任务。" }
                            }
                        }
                    }
                },
                "PackageError": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string" },
                        "error": { "type": "string" },
                        "expected_pattern": { "type": "string" },
                        "example": { "type": "string" }
                    }
                }
            }
        }
    })
}
#[allow(dead_code)]
fn legacy_openapi_docs_public_html() -> String {
    r###"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Easy Deploy 版本包投递接口</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f5f7fb;
      --panel: #ffffff;
      --text: #182230;
      --muted: #667085;
      --line: #d9e0ec;
      --brand: #1f6feb;
      --brand-soft: #e8f1ff;
      --code-bg: #101828;
      --code-text: #e6edf6;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      line-height: 1.7;
    }
    .page { max-width: 1120px; margin: 0 auto; padding: 32px 20px 56px; }
    .hero {
      padding: 28px 30px;
      border: 1px solid var(--line);
      border-radius: 12px;
      background: linear-gradient(135deg, #fff 0%, #eef5ff 100%);
    }
    .eyebrow { margin: 0 0 10px; color: var(--brand); font-size: 13px; font-weight: 700; }
    h1 { margin: 0; font-size: clamp(28px, 4vw, 44px); line-height: 1.14; letter-spacing: 0; }
    .summary { max-width: 820px; margin: 14px 0 0; color: var(--muted); font-size: 16px; }
    .layout { display: grid; grid-template-columns: 260px minmax(0, 1fr); gap: 22px; margin-top: 22px; align-items: start; }
    nav, section {
      border: 1px solid var(--line);
      border-radius: 10px;
      background: var(--panel);
    }
    nav { position: sticky; top: 16px; padding: 16px; }
    nav strong { display: block; margin-bottom: 10px; }
    nav a { display: block; padding: 8px 10px; border-radius: 8px; color: #344054; text-decoration: none; }
    nav a:hover { background: var(--brand-soft); color: var(--brand); }
    article { display: grid; gap: 18px; }
    section { padding: 22px 24px; }
    h2 { margin: 0 0 12px; font-size: 21px; letter-spacing: 0; }
    h3 { margin: 18px 0 8px; font-size: 16px; letter-spacing: 0; }
    p, ul, ol { margin-top: 0; }
    li + li { margin-top: 6px; }
    code { padding: 2px 5px; border-radius: 5px; background: #eef2f7; color: #174a83; font-family: "JetBrains Mono", Consolas, monospace; font-size: 0.92em; }
    pre { margin: 12px 0 0; overflow: auto; border-radius: 10px; background: var(--code-bg); }
    pre code { display: block; padding: 18px; background: transparent; color: var(--code-text); line-height: 1.7; white-space: pre; }
    .callout { padding: 14px 16px; border-radius: 10px; background: var(--brand-soft); color: #18406f; }
    .endpoint { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; margin: 10px 0 14px; }
    .method { padding: 4px 8px; border-radius: 6px; background: #16a34a; color: #fff; font-size: 12px; font-weight: 800; }
    .field-list { display: grid; gap: 10px; }
    .field { padding: 12px; border: 1px solid var(--line); border-radius: 8px; background: #fbfcff; }
    .field strong { display: block; margin-bottom: 4px; }
    @media (max-width: 860px) {
      .layout { grid-template-columns: 1fr; }
      nav { position: static; }
    }
  </style>
</head>
<body>
  <div class="page">
    <header class="hero">
      <p class="eyebrow">Easy Deploy OpenAPI</p>
      <h1>版本包投递接口</h1>
      <p class="summary">这份文档无需登录即可访问。Easy Deploy 对外只提供版本包上传能力；应用、节点、环境变量、Compose、脚本阶段、立即发布或定时发布都在后台页面配置。</p>
    </header>
    <div class="layout">
      <nav aria-label="接口文档目录">
        <strong>目录</strong>
        <a href="#scope">职责边界</a>
        <a href="#prepare">接入准备</a>
        <a href="#package-name">包名规范</a>
        <a href="#endpoint">接口</a>
        <a href="#fields">字段</a>
        <a href="#example">示例</a>
        <a href="#errors">错误处理</a>
      </nav>
      <article>
        <section id="scope">
          <h2>职责边界</h2>
          <div class="callout">业务项目或 AI 只负责构建并上传版本包，不创建应用、不更新部署配置、不触发部署、不轮询任务。</div>
          <ul>
            <li>后台先创建应用并配置目标节点、Compose、环境变量、部署脚本和健康检查。</li>
            <li>后台选择发布策略：自动上传即入队、手动发布或定时发布。</li>
            <li>上传接口只登记版本包并返回 release/queue 信息；真正发布过程由平台队列按应用串行执行。</li>
          </ul>
        </section>
        <section id="prepare">
          <h2>接入准备</h2>
          <ol>
            <li>在后台确认服务标识，例如 <code>qfy-sc-test-backend</code>。</li>
            <li>在 API Token 页面创建 token，并赋予版本包上传权限。</li>
            <li>业务项目本地或 CI 构建版本包，文件名必须包含服务标识和版本号。</li>
            <li>调用上传接口，把包投递到 Easy Deploy。</li>
          </ol>
        </section>
        <section id="package-name">
          <h2>包名规范</h2>
          <p>推荐格式：</p>
          <pre><code>{service_key}_version_{x_y_z}.tar.gz</code></pre>
          <p>示例：</p>
          <pre><code>qfy-sc-test-backend_version_1_2_3.tar.gz
qfy-sc-test-admin_version_v1.2.3.tar.gz</code></pre>
          <p>如果包名前缀和路径中的 <code>service_key</code> 不一致，接口会返回 400。</p>
        </section>
        <section id="endpoint">
          <h2>接口</h2>
          <div class="endpoint"><span class="method">POST</span><code>/api/v1/services/{service_key}/packages</code></div>
          <p>认证方式：</p>
          <pre><code>Authorization: Bearer &lt;API_TOKEN&gt;</code></pre>
          <p>请求类型：</p>
          <pre><code>multipart/form-data</code></pre>
        </section>
        <section id="fields">
          <h2>字段</h2>
          <div class="field-list">
            <div class="field"><strong><code>package_file</code> 必填</strong>版本包文件。兼容字段名：<code>file</code>、<code>artifact_file</code>。</div>
            <div class="field"><strong><code>release_version</code> 可选</strong>显式版本号。填写时必须和包名解析出的版本一致。兼容字段名：<code>artifact_version</code>。</div>
            <div class="field"><strong><code>version_code</code> 可选</strong>版本排序号。不填时平台从包名版本解析。兼容字段名：<code>versionCode</code>。</div>
            <div class="field"><strong><code>published_at</code> 可选</strong>业务项目构建或发布时间。兼容字段名：<code>publishedAt</code>。</div>
            <div class="field"><strong><code>source</code> 可选</strong>来源标记，例如 <code>ci</code>、<code>local-script</code>、<code>ai-agent</code>。</div>
          </div>
        </section>
        <section id="example">
          <h2>示例</h2>
          <pre><code>curl -X POST "$EASY_DEPLOY_URL/api/v1/services/qfy-sc-test-backend/packages" \
  -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  -F "package_file=@qfy-sc-test-backend_version_1_2_3.tar.gz" \
  -F "source=ci" \
  -F "published_at=2026-07-03T10:00:00+08:00"</code></pre>
          <p>成功响应会包含 <code>release_id</code>、<code>queue_id</code>、<code>release_version</code>、<code>config_revision</code>。如果应用设置为非自动入队，<code>queue_id</code> 可能为空。</p>
        </section>
        <section id="errors">
          <h2>错误处理</h2>
          <ul>
            <li><code>400</code>：包名不符合规范、版本冲突、文件为空或字段读取失败。</li>
            <li><code>401</code>：没有提供有效 API Token。</li>
            <li><code>403</code>：Token 没有版本包上传权限。</li>
            <li><code>404</code>：服务标识不存在，需要先在后台创建并配置应用。</li>
          </ul>
          <p>包名错误会返回 <code>expected_pattern</code> 和 <code>example</code>，调用方可以直接把错误内容展示给开发者或 AI。</p>
        </section>
      </article>
    </div>
  </div>
</body>
</html>"###.to_owned()
}

fn openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Easy Deploy Package Upload API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Easy Deploy OpenAPI 只提供版本包投递能力。应用、节点、环境变量、Compose 配置、部署脚本、自动/手动/定时发布策略都在平台后台维护。推荐流程是申请 OSS 直传地址、PUT 上传到 OSS、完成登记。"
        },
        "servers": [
            { "url": "http://127.0.0.1:9066", "description": "本机默认地址" },
            { "url": "https://easy-deploy.quanxinfu.com", "description": "当前正式环境" }
        ],
        "security": [{ "BearerAuth": [] }],
        "paths": {
            "/api/v1/apps/{app_key}/units/{unit_key}/releases": {
                "post": {
                    "operationId": "uploadDeploymentUnitRelease",
                    "summary": "原子上传部署单元版本与发布包",
                    "description": "上传成功后部署单元 version、平台生成的 versionCode 与唯一发布包不可分离。只登记制品，不触发部署。version 必须是无 v 前缀的 x.y.z。",
                    "parameters": [
                        { "name": "app_key", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "unit_key", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "Idempotency-Key", "in": "header", "required": true, "schema": { "type": "string", "maxLength": 128 } }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "multipart/form-data": {
                                "schema": {
                                    "type": "object",
                                    "required": ["artifact_version", "package_file"],
                                    "properties": {
                                        "artifact_version": { "type": "string", "pattern": "^[0-9]+\\.[0-9]+\\.[0-9]+$", "examples": ["1.2.3"] },
                                        "package_file": { "type": "string", "contentMediaType": "application/octet-stream" },
                                        "published_at": { "type": "string", "format": "date-time" },
                                        "source": { "type": "string", "examples": ["ci"] }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "201": { "description": "部署单元版本和制品已原子登记" },
                        "400": { "description": "版本、包或 Idempotency-Key 无效" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "description": "权限或 app/unit scope 不允许" },
                        "409": { "description": "版本已存在或幂等内容冲突" }
                    }
                }
            },
            "/api/v1/apps/{app_key}/releases": {
                "post": {
                    "operationId": "createApplicationRelease",
                    "summary": "创建不可变应用发布版本",
                    "description": "可基于任意历史应用版本，只提交变化单元；平台展开为完整单元与环境配置快照。只创建 ready 版本，不触发部署。",
                    "parameters": [
                        { "name": "app_key", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "Idempotency-Key", "in": "header", "required": true, "schema": { "type": "string", "maxLength": 128 } }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["version"],
                                    "properties": {
                                        "version": { "type": "string", "pattern": "^[0-9]+\\.[0-9]+\\.[0-9]+$" },
                                        "base_app_release_id": { "type": ["integer", "null"] },
                                        "unit_changes": { "type": "array", "items": { "type": "object" } },
                                        "environment_configs": { "type": "array", "items": { "type": "object" } }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "201": { "description": "完整不可变应用版本已创建，deployment_started 固定为 false" },
                        "400": { "description": "缺少部署单元、环境配置或版本格式错误" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "description": "权限或资源 scope 不允许" },
                        "409": { "description": "版本或 Idempotency-Key 冲突" }
                    }
                }
            },
            "/api/v1/services/{service_key}/packages/uploads": {
                "post": {
                    "operationId": "createServicePackageUpload",
                    "summary": "申请版本包 OSS 直传地址",
                    "description": "校验服务标识和包名后，返回一次性 OSS PUT 签名 URL。调用方必须携带 upload.headers 的全部请求头；此接口不会创建 release，也不会触发发布。调用方先把文件 PUT 到 upload.url，再调用 complete 接口登记；若 OSS 返回对象版本号，平台会在完成校验后绑定该版本号，并在部署时签名下载已校验版本。",
                    "parameters": [{ "$ref": "#/components/parameters/ServiceKey" }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreatePackageUploadRequest" },
                                "examples": {
                                    "default": {
                                        "value": {
                                            "file_name": "orders-api-prod_version_1_2_3.tar.gz",
                                            "source": "ai-agent",
                                            "published_at": "2026-07-09T10:00:00+08:00"
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "上传地址已创建",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/CreatePackageUploadResponse" }
                                }
                            }
                        },
                        "400": { "$ref": "#/components/responses/PackageBadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "404": { "description": "service_key 不存在" }
                    }
                }
            },
            "/api/v1/services/{service_key}/packages/uploads/{upload_id}/complete": {
                "post": {
                    "operationId": "completeServicePackageUpload",
                    "summary": "完成版本包上传登记",
                    "description": "调用方完成 OSS PUT 后，提交 SHA-256 和文件大小作为断言；平台会重新读取 OSS 对象并计算 SHA-256 和字节数，只有两者一致才登记 release。当前单个对象上限为 5 GiB。断言不一致时不会消费上传会话，调用方可使用正确值重试。随后平台保存配置快照，并按应用的发布设置决定是否自动进入串行发布队列。",
                    "parameters": [
                        { "$ref": "#/components/parameters/ServiceKey" },
                        {
                            "name": "upload_id",
                            "in": "path",
                            "required": true,
                            "schema": { "type": "string" },
                            "description": "申请上传地址接口返回的 upload_id。"
                        }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CompletePackageUploadRequest" },
                                "examples": {
                                    "default": {
                                        "value": {
                                            "checksum_sha256": "64位小写sha256",
                                            "size_bytes": 10485760,
                                            "source": "ai-agent"
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "版本包已登记",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/UploadServicePackageResponse" }
                                }
                            }
                        },
                        "400": { "$ref": "#/components/responses/PackageBadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" },
                        "409": { "description": "上传会话已完成、过期或不可用" }
                    }
                }
            },
            "/api/v1/services/{service_key}/packages": {
                "post": {
                    "operationId": "uploadServicePackageLegacy",
                    "summary": "兼容：multipart 直接上传到平台",
                    "description": "保留给旧脚本使用。新项目建议使用 OSS 直传接口，避免大文件经过 easy-deploy 后台进程。",
                    "parameters": [{ "$ref": "#/components/parameters/ServiceKey" }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "multipart/form-data": {
                                "schema": { "$ref": "#/components/schemas/UploadServicePackageRequest" }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "版本包已登记",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/UploadServicePackageResponse" }
                                }
                            }
                        },
                        "400": { "$ref": "#/components/responses/PackageBadRequest" },
                        "401": { "$ref": "#/components/responses/Unauthorized" },
                        "403": { "$ref": "#/components/responses/Forbidden" }
                    }
                }
            }
        },
        "components": {
            "securitySchemes": {
                "BearerAuth": { "type": "http", "scheme": "bearer" }
            },
            "parameters": {
                "ServiceKey": {
                    "name": "service_key",
                    "in": "path",
                    "required": true,
                    "schema": { "type": "string" },
                    "description": "后台应用标识，必须与版本包文件名前缀一致。"
                }
            },
            "responses": {
                "PackageBadRequest": {
                    "description": "包名、字段或上传会话校验失败",
                    "content": {
                        "application/json": {
                            "schema": {
                                "oneOf": [
                                    { "$ref": "#/components/schemas/PackageError" },
                                    { "$ref": "#/components/schemas/ApiError" }
                                ]
                            }
                        }
                    }
                },
                "Unauthorized": { "description": "缺少或无效 API Token" },
                "Forbidden": { "description": "API Token 没有版本包上传权限" }
            },
            "schemas": {
                "CreatePackageUploadRequest": {
                    "type": "object",
                    "required": ["file_name"],
                    "properties": {
                        "file_name": { "type": "string", "description": "版本包文件名，例如 orders-api-prod_version_1_2_3.tar.gz。兼容 fileName。" },
                        "release_version": { "type": "string", "description": "可选。显式版本号，必须与包名解析结果一致。兼容 releaseVersion、artifact_version。" },
                        "version_code": { "type": "integer", "description": "可选。版本排序号；不传时平台按 vX.Y.Z 解析。兼容 versionCode。" },
                        "published_at": { "type": "string", "description": "可选。构建或发布时间，ISO-8601。兼容 publishedAt。" },
                        "source": { "type": "string", "description": "可选。来源标记，例如 ai-agent、local-script、ci。" }
                    }
                },
                "CreatePackageUploadResponse": {
                    "type": "object",
                    "properties": {
                        "data": {
                            "type": "object",
                            "properties": {
                                "app_id": { "type": "integer" },
                                "service_key": { "type": "string" },
                                "upload_id": { "type": "string" },
                                "release_version": { "type": "string" },
                                "version_code": { "type": "integer" },
                                "versionCode": { "type": "integer" },
                                "file_name": { "type": "string" },
                                "object_key": { "type": "string" },
                                "bucket": { "type": "string" },
                                "endpoint": { "type": "string" },
                                "upload": {
                                    "type": "object",
                                    "properties": {
                                        "method": { "type": "string", "enum": ["PUT"] },
                                        "url": { "type": "string" },
                                        "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                                        "expires_at": { "type": "string" },
                                        "expiresAt": { "type": "string" }
                                    }
                                },
                                "complete_path": { "type": "string" }
                            }
                        }
                    }
                },
                "CompletePackageUploadRequest": {
                    "type": "object",
                    "required": ["checksum_sha256", "size_bytes"],
                    "properties": {
                        "checksum_sha256": { "type": "string", "description": "上传文件的 64 位 SHA-256 hex，作为服务端 OSS 校验结果的断言；断言不一致时可用正确值重试。兼容 checksumSha256。" },
                        "size_bytes": { "type": "integer", "description": "上传文件字节数，作为服务端 OSS 校验结果的断言；断言不一致时可用正确值重试。兼容 sizeBytes。" },
                        "published_at": { "type": "string", "description": "可选，覆盖申请上传地址时的 published_at。兼容 publishedAt。" },
                        "source": { "type": "string", "description": "可选，覆盖申请上传地址时的来源标记。" }
                    }
                },
                "UploadServicePackageRequest": {
                    "type": "object",
                    "required": ["package_file"],
                    "properties": {
                        "package_file": { "type": "string", "format": "binary", "description": "版本包文件。兼容 file、artifact_file。" },
                        "release_version": { "type": "string" },
                        "version_code": { "type": "integer" },
                        "published_at": { "type": "string" },
                        "entry_file": { "type": "string" },
                        "source": { "type": "string" }
                    }
                },
                "UploadServicePackageResponse": {
                    "type": "object",
                    "properties": {
                        "data": {
                            "type": "object",
                            "properties": {
                                "app_id": { "type": "integer" },
                                "service_key": { "type": "string" },
                                "release_id": { "type": "integer" },
                                "queue_id": { "type": ["integer", "null"] },
                                "release_version": { "type": "string" },
                                "version_code": { "type": "integer" },
                                "versionCode": { "type": "integer" },
                                "published_at": { "type": "string" },
                                "publishedAt": { "type": "string" },
                                "package_path": { "type": "string" },
                                "package_kind": { "type": "string" },
                                "config_snapshot_id": { "type": "integer" },
                                "config_revision_no": { "type": "integer" },
                                "config_revision": { "type": "string" },
                                "queued": { "type": "boolean" },
                                "task_id": { "type": ["integer", "null"], "description": "上传接口不直接创建部署任务。" }
                            }
                        }
                    }
                },
                "PackageError": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string" },
                        "error": { "type": "string" },
                        "expected_pattern": { "type": "string" },
                        "example": { "type": "string" }
                    }
                },
                "ApiError": {
                    "type": "object",
                    "properties": { "error": { "type": "string" } }
                }
            }
        }
    })
}

fn openapi_docs_public_html() -> String {
    r###"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Easy Deploy OpenAPI</title>
  <style>
    :root { color-scheme: light; --bg:#f6f8fc; --panel:#fff; --text:#172033; --muted:#667085; --line:#d8e0ef; --brand:#2563eb; --soft:#eef4ff; --code:#0b1220; --codeText:#e6edf7; }
    * { box-sizing: border-box; }
    body { margin:0; background:var(--bg); color:var(--text); font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; line-height:1.72; }
    .page { max-width:1180px; margin:0 auto; padding:28px 20px 56px; }
    .hero { border:1px solid var(--line); border-radius:10px; background:linear-gradient(135deg,#fff 0%,#eff6ff 100%); padding:26px 28px; }
    .eyebrow { margin:0 0 8px; color:var(--brand); font-size:13px; font-weight:800; }
    h1 { margin:0; font-size:36px; line-height:1.15; letter-spacing:0; }
    .summary { margin:12px 0 0; max-width:880px; color:var(--muted); }
    .layout { display:grid; grid-template-columns:260px minmax(0,1fr); gap:20px; margin-top:20px; align-items:start; }
    nav, section { border:1px solid var(--line); border-radius:10px; background:var(--panel); }
    nav { position:sticky; top:16px; padding:14px; }
    nav strong { display:block; margin:4px 8px 10px; }
    nav a { display:block; padding:8px 10px; border-radius:7px; color:#344054; text-decoration:none; }
    nav a:hover { background:var(--soft); color:var(--brand); }
    article { display:grid; gap:16px; }
    section { padding:22px 24px; }
    h2 { margin:0 0 12px; font-size:22px; letter-spacing:0; }
    h3 { margin:18px 0 8px; font-size:16px; letter-spacing:0; }
    p, ol, ul { margin-top:0; }
    li + li { margin-top:6px; }
    code { padding:2px 5px; border-radius:5px; background:#eef2f7; color:#174a83; font-family:"JetBrains Mono", Consolas, monospace; font-size:.92em; }
    pre { margin:12px 0 0; overflow:auto; border-radius:9px; background:var(--code); }
    pre code { display:block; padding:16px; color:var(--codeText); background:transparent; white-space:pre; }
    .callout { padding:13px 15px; border-radius:8px; background:var(--soft); color:#17406e; }
    .endpoint { display:flex; align-items:center; gap:10px; flex-wrap:wrap; margin:10px 0 12px; }
    .method { padding:4px 8px; border-radius:6px; background:#16a34a; color:#fff; font-size:12px; font-weight:800; }
    .grid { display:grid; grid-template-columns:repeat(2,minmax(0,1fr)); gap:12px; }
    .field { padding:12px; border:1px solid var(--line); border-radius:8px; background:#fbfcff; }
    .field strong { display:block; margin-bottom:4px; }
    @media (max-width:860px) { .layout, .grid { grid-template-columns:1fr; } nav { position:static; } h1 { font-size:30px; } }
  </style>
</head>
<body>
  <div class="page">
    <header class="hero">
      <p class="eyebrow">Easy Deploy OpenAPI</p>
      <h1>版本包与应用版本接口</h1>
      <p class="summary">这份文档无需登录即可访问，面向业务项目 CI 和 AI。OpenAPI 原子登记部署单元版本与发布包，并显式创建不可变应用版本；部署仍只能由运维人员在后台手动启动。</p>
    </header>
    <div class="layout">
      <nav>
        <strong>目录</strong>
        <a href="#scope">职责边界</a>
        <a href="#prepare">接入准备</a>
        <a href="#multi-unit">多单元发布</a>
        <a href="#naming">包名规范</a>
        <a href="#flow">推荐流程</a>
        <a href="#api-create">申请上传地址</a>
        <a href="#api-complete">完成登记</a>
        <a href="#script">脚本示例</a>
        <a href="#legacy">兼容接口</a>
        <a href="#errors">错误处理</a>
      </nav>
      <article>
        <section id="scope">
          <h2>职责边界</h2>
          <div class="callout">外部项目不能通过 OpenAPI 创建应用、修改配置或触发部署。CI 只能上传部署单元版本包并创建不可变应用版本，运维人员在后台预览后手动部署。</div>
          <ul>
            <li>后台配置：应用标识、环境、目标节点、Compose 内容、环境变量、部署脚本、健康检查和发布策略。</li>
            <li>业务项目：构建版本包、计算 SHA-256、调用 OpenAPI 上传并完成登记。</li>
            <li>发布执行：平台按应用维度串行处理版本，部署时目标节点直接从 OSS 下载版本包。</li>
          </ul>
        </section>
        <section id="multi-unit">
          <h2>多单元发布</h2>
          <ol>
            <li>为每个发生变化的模块调用 <code>POST /api/v1/apps/{app_key}/units/{unit_key}/releases</code>，同时上传 <code>x.y.z</code> 版本和唯一发布包。</li>
            <li>收集响应中的 <code>unit_release_id</code>，调用 <code>POST /api/v1/apps/{app_key}/releases</code> 创建应用版本。</li>
            <li>增量应用版本填写 <code>base_app_release_id</code> 并只提交变化单元；平台返回展开后的完整 manifest。</li>
            <li>两个接口都必须发送 <code>Idempotency-Key</code>。同 key 同内容返回原结果，同 key 异内容返回 <code>IDEMPOTENCY_CONFLICT</code>。</li>
          </ol>
          <pre><code>curl -X POST "$EASY_DEPLOY_URL/api/v1/apps/voucher-hub/units/api/releases" \
  -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  -H "Idempotency-Key: api-1.4.0" \
  -F "artifact_version=1.4.0" \
  -F "package_file=@api-1.4.0.tar.gz"</code></pre>
          <p>创建应用版本的成功响应包含 <code>manifest_hash</code>、应用 <code>versionCode</code> 和完整单元摘要，并固定返回 <code>deployment_started=false</code>。</p>
        </section>
        <section id="prepare">
          <h2>接入准备</h2>
          <ol>
            <li>在 easy-deploy 后台创建应用，记录应用标识，例如 <code>orders-api-prod</code>。</li>
            <li>在设置页配置制品存储为阿里云 OSS，包括 region、endpoint、bucket、object prefix、AccessKey ID 和 Secret。</li>
            <li>在 API Token 页面创建 token，并确保 token 拥有版本包上传权限。</li>
            <li>业务项目按 <code>{service_key}_version_{x_y_z}.tar.gz</code> 命名版本包。</li>
          </ol>
        </section>
        <section id="naming">
          <h2>包名规范</h2>
          <pre><code>{service_key}_version_{x_y_z}.tar.gz</code></pre>
          <p>示例：</p>
          <pre><code>orders-api-prod_version_1_2_3.tar.gz
orders-api-prod_version_v1.2.3.tar.gz</code></pre>
          <p><code>service_key</code> 必须等于路径中的服务标识。版本会规范化为 <code>v1.2.3</code>，未传 <code>versionCode</code> 时平台会解析为 <code>1002003</code>。</p>
        </section>
        <section id="flow">
          <h2>推荐流程</h2>
          <ol>
            <li>调用 <code>POST /api/v1/services/{service_key}/packages/uploads</code>，拿到 OSS <code>PUT</code> 签名 URL。</li>
            <li>按返回的 <code>upload.method</code>、<code>upload.url</code> 和 <code>upload.headers</code> 把版本包直传 OSS。</li>
            <li>计算本地文件 SHA-256 和字节数作为断言，调用 complete 接口；平台会重新读取 OSS 对象复核。</li>
            <li>如果应用开启“自动上传即入队”，平台会把 release 加入串行发布队列；否则在发布版本页手动或定时发布。</li>
          </ol>
        </section>
        <section id="api-create">
          <h2>申请上传地址</h2>
          <div class="endpoint"><span class="method">POST</span><code>/api/v1/services/{service_key}/packages/uploads</code></div>
          <p>若 OSS 返回可固定的对象版本号，完成登记时平台会保存它；后续部署会签名下载该已验证版本。若返回 <code>null</code>，说明 Bucket 可能处于暂停版本控制状态，平台会拒绝完成登记；请调整 Bucket 配置后重新申请并上传版本包。</p>
          <p>平台配置的 AccessKey 需要具备对应 Bucket 的 <code>oss:PutObject</code>、<code>oss:GetObject</code>、<code>oss:DeleteObject</code> 和 <code>oss:ListObjectVersions</code> 权限；开启版本控制时还需具备 <code>oss:GetObjectVersion</code> 和 <code>oss:DeleteObjectVersion</code>，并为未完成上传路径配置版本生命周期清理。</p>
          <pre><code>Authorization: Bearer &lt;API_TOKEN&gt;
Content-Type: application/json</code></pre>
          <div class="grid">
            <div class="field"><strong><code>file_name</code> 必填</strong>版本包文件名。兼容 <code>fileName</code>。</div>
            <div class="field"><strong><code>release_version</code> 可选</strong>显式版本号，必须与包名一致。兼容 <code>releaseVersion</code>、<code>artifact_version</code>。</div>
            <div class="field"><strong><code>version_code</code> 可选</strong>版本排序号。兼容 <code>versionCode</code>。</div>
            <div class="field"><strong><code>source</code> 可选</strong>来源标记，例如 <code>ai-agent</code>、<code>local-script</code>、<code>ci</code>。</div>
          </div>
          <pre><code>{
  "file_name": "orders-api-prod_version_1_2_3.tar.gz",
  "source": "ai-agent",
  "published_at": "2026-07-09T10:00:00+08:00"
}</code></pre>
        </section>
        <section id="api-complete">
          <h2>完成登记</h2>
          <div class="endpoint"><span class="method">POST</span><code>/api/v1/services/{service_key}/packages/uploads/{upload_id}/complete</code></div>
          <p>只有 OSS PUT 成功后才调用。平台会重新读取对象并校验 SHA-256 和字节数一致后，才登记 release、记录配置快照，并按应用发布策略处理入队；单个对象上限为 5 GiB。断言填错时可用正确值重试同一个上传会话。</p>
          <div class="grid">
            <div class="field"><strong><code>checksum_sha256</code> 必填</strong>64 位 SHA-256 hex，作为 OSS 复核结果的断言。兼容 <code>checksumSha256</code>。</div>
            <div class="field"><strong><code>size_bytes</code> 必填</strong>上传文件字节数，作为 OSS 复核结果的断言。兼容 <code>sizeBytes</code>。</div>
            <div class="field"><strong><code>published_at</code> 可选</strong>覆盖申请上传地址时的发布时间。</div>
            <div class="field"><strong><code>source</code> 可选</strong>覆盖申请上传地址时的来源标记。</div>
          </div>
        </section>
        <section id="script">
          <h2>脚本示例</h2>
          <pre><code>#!/usr/bin/env bash
set -euo pipefail

EASY_DEPLOY_URL="${EASY_DEPLOY_URL:-https://easy-deploy.quanxinfu.com}"
SERVICE_KEY="orders-api-prod"
PACKAGE="orders-api-prod_version_1_2_3.tar.gz"
SHA256="$(sha256sum "$PACKAGE" | awk '{print $1}')"
SIZE_BYTES="$(wc -c &lt; "$PACKAGE" | tr -d ' ')"

CREATE_JSON="$(curl -fsS -X POST "$EASY_DEPLOY_URL/api/v1/services/$SERVICE_KEY/packages/uploads" \
  -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"file_name\":\"$PACKAGE\",\"source\":\"local-script\"}")"

UPLOAD_URL="$(printf '%s' "$CREATE_JSON" | jq -r '.data.upload.url')"
UPLOAD_ID="$(printf '%s' "$CREATE_JSON" | jq -r '.data.upload_id')"

curl -fsS -X PUT "$UPLOAD_URL" \
  -H "Content-Type: application/octet-stream" \
  -H "x-oss-forbid-overwrite: true" \
  --data-binary "@$PACKAGE"

curl -fsS -X POST "$EASY_DEPLOY_URL/api/v1/services/$SERVICE_KEY/packages/uploads/$UPLOAD_ID/complete" \
  -H "Authorization: Bearer $EASY_DEPLOY_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"checksum_sha256\":\"$SHA256\",\"size_bytes\":$SIZE_BYTES,\"source\":\"local-script\"}"</code></pre>
        </section>
        <section id="legacy">
          <h2>兼容接口</h2>
          <p>旧脚本仍可使用 <code>POST /api/v1/services/{service_key}/packages</code> 以 <code>multipart/form-data</code> 直接上传到平台。新项目建议使用 OSS 直传，避免大文件经过 easy-deploy 进程。</p>
          <p>升级前登记且未保存对象版本号的 OSS 制品会被标记为历史未绑定制品，平台会阻止其部署以避免下载被改写的对象；请使用当前直传流程重新上传相同版本或新的版本。</p>
        </section>
        <section id="errors">
          <h2>错误处理</h2>
          <ul>
            <li><code>400</code>：包名不符合规范、服务标识不匹配、checksum/size 无效、上传会话过期。</li>
            <li><code>401</code>：缺少或无效 API Token。</li>
            <li><code>403</code>：Token 没有版本包上传权限。</li>
            <li><code>409</code>：上传会话已经完成或不可用。</li>
          </ul>
          <p>包名错误会返回 <code>code</code>、<code>expected_pattern</code> 和 <code>example</code>，业务项目或 AI 可以直接把错误展示给开发者。</p>
        </section>
      </article>
    </div>
  </div>
</body>
</html>"###.to_owned()
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
            .unwrap_or_else(|| "未知".to_owned()),
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
                nav_item("发布版本", "/artifacts", "artifacts", active_path),
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
                nav_item("事件日志", "/events", "events", active_path),
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

fn default_if_blank<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn default_i64(value: i64, fallback: i64) -> i64 {
    if value == 0 { fallback } else { value }
}

fn parse_create_app_form(bytes: &[u8]) -> Result<CreateAppForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(CreateAppForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        app_key: required_form_value(&fields, "app_key")?,
        name: required_form_value(&fields, "name")?,
        description: first_form_value(&fields, "description"),
        environment: first_form_value(&fields, "environment"),
        deploy_strategy: first_form_value(&fields, "deploy_strategy"),
        release_source: first_form_value(&fields, "release_source"),
        auto_queue_release: form_bool(&fields, "auto_queue_release"),
        work_dir: first_form_value(&fields, "work_dir"),
        compose_content: first_form_value(&fields, "compose_content"),
        env_content: first_form_value(&fields, "env_content"),
        deploy_script_pre_deploy: first_form_value(&fields, "deploy_script_pre_deploy"),
        deploy_script_deploy: first_form_value(&fields, "deploy_script_deploy"),
        deploy_script_post_deploy: first_form_value(&fields, "deploy_script_post_deploy"),
        deploy_script_switch_traffic: first_form_value(&fields, "deploy_script_switch_traffic"),
        deploy_script_cleanup: first_form_value(&fields, "deploy_script_cleanup"),
        health_check_kind: first_form_value(&fields, "health_check_kind"),
        health_endpoint: first_form_value(&fields, "health_endpoint"),
        health_timeout_secs: optional_form_i64(&fields, "health_timeout_secs")?,
        health_expected_status: optional_form_i64(&fields, "health_expected_status")?,
        target_node_ids: parse_form_ids(&fields, "target_node_ids")?,
    })
}

fn parse_update_app_metadata_form(bytes: &[u8]) -> Result<UpdateAppMetadataForm, String> {
    let fields = parse_urlencoded_fields(bytes);
    Ok(UpdateAppMetadataForm {
        csrf_token: required_form_value(&fields, "csrf_token")?,
        name: required_form_value(&fields, "name")?,
        description: first_form_value(&fields, "description"),
        environment: first_form_value(&fields, "environment"),
        work_dir: required_form_value(&fields, "work_dir")?,
        deploy_strategy: first_form_value(&fields, "deploy_strategy"),
        release_source: first_form_value(&fields, "release_source"),
        auto_queue_release: form_bool(&fields, "auto_queue_release"),
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
        Some("deleted") => Some("API Token 已删除。"),
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
            "安装完成后先刷新节点能力，再回到部署确认页提交任务",
        )
    } else if path.starts_with("/tasks/") {
        (
            "返回来源任务",
            "重新探测并返回任务",
            "安装完成后先刷新节点能力，再回到来源任务查看修复结果",
        )
    } else if path.starts_with("/nodes/") {
        (
            "返回节点详情",
            "重新探测并返回节点详情",
            "安装完成后刷新节点能力，并回到节点详情确认组件状态",
        )
    } else if path.starts_with("/services/") {
        (
            "返回运行项日志",
            "重新探测并返回运行项日志",
            "处理完成后回到运行项日志，继续查看该节点的运行上下文",
        )
    } else if path == "/services" {
        (
            "返回运行项列表",
            "重新探测并返回运行项列表",
            "处理完成后回到运行项列表，继续查看运行项和节点状态",
        )
    } else {
        (
            "返回上一页",
            "重新探测并返回",
            "安装完成后刷新节点能力，再回到来源页面继续操作",
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

fn api_error_code(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ApiStableErrorBody {
            code,
            error: message,
        }),
    )
        .into_response()
}

fn application_release_api_error(error: ApplicationReleaseError) -> Response {
    let (status, code) = match &error {
        ApplicationReleaseError::Validation(_) => (StatusCode::BAD_REQUEST, "VALIDATION_ERROR"),
        ApplicationReleaseError::Conflict(_) => (StatusCode::CONFLICT, "VERSION_CONFLICT"),
        ApplicationReleaseError::NotFound(_) => (StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND"),
        ApplicationReleaseError::Database(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "DATABASE_ERROR")
        }
    };
    api_error_code(status, code, &error.to_string())
}

#[allow(clippy::result_large_err)]
fn required_idempotency_key(headers: &HeaderMap) -> Result<String, Response> {
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(api_error_code(
            StatusCode::BAD_REQUEST,
            "INVALID_IDEMPOTENCY_KEY",
            "Idempotency-Key is required and must be at most 128 visible characters",
        ));
    }
    Ok(key.to_owned())
}

fn stable_request_hash(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

async fn idempotency_replay(
    db: &SqlitePool,
    token_id: i64,
    action: &str,
    key: &str,
    request_hash: &str,
) -> Result<Option<Response>, Response> {
    if let Err(error) = sqlx::query(
        "DELETE FROM api_idempotency_records WHERE expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .execute(db)
    .await
    {
        return Err(api_error_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DATABASE_ERROR",
            &error.to_string(),
        ));
    }
    let record = sqlx::query_as::<_, (String, i64, String)>(
        r#"
        SELECT request_hash, response_status, response_body
        FROM api_idempotency_records
        WHERE token_id = ?1 AND action = ?2 AND idempotency_key = ?3
        "#,
    )
    .bind(token_id)
    .bind(action)
    .bind(key)
    .fetch_optional(db)
    .await
    .map_err(|error| {
        api_error_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DATABASE_ERROR",
            &error.to_string(),
        )
    })?;
    let Some((stored_hash, response_status, response_body)) = record else {
        return Ok(None);
    };
    if stored_hash != request_hash {
        return Err(api_error_code(
            StatusCode::CONFLICT,
            "IDEMPOTENCY_CONFLICT",
            "Idempotency-Key was already used with different request content",
        ));
    }
    let status = StatusCode::from_u16(response_status as u16).unwrap_or(StatusCode::OK);
    let body = serde_json::from_str::<serde_json::Value>(&response_body).map_err(|error| {
        api_error_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "IDEMPOTENCY_RECORD_CORRUPTED",
            &error.to_string(),
        )
    })?;
    Ok(Some((status, Json(body)).into_response()))
}

#[allow(clippy::too_many_arguments)]
async fn store_idempotency_response(
    db: &SqlitePool,
    token_id: i64,
    action: &str,
    key: &str,
    request_hash: &str,
    resource_type: &str,
    resource_id: &str,
    status: StatusCode,
    body: &serde_json::Value,
) -> Result<(), Response> {
    let body = serde_json::to_string(body).map_err(|error| {
        api_error_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "SERIALIZATION_ERROR",
            &error.to_string(),
        )
    })?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO api_idempotency_records(
            token_id, action, idempotency_key, request_hash, resource_type,
            resource_id, response_status, response_body, expires_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
            strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+24 hours')
        )
        ON CONFLICT(token_id, action, idempotency_key) DO NOTHING
        "#,
    )
    .bind(token_id)
    .bind(action)
    .bind(key)
    .bind(request_hash)
    .bind(resource_type)
    .bind(resource_id)
    .bind(i64::from(status.as_u16()))
    .bind(body)
    .execute(db)
    .await
    .map_err(|error| {
        api_error_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DATABASE_ERROR",
            &error.to_string(),
        )
    })?;
    if inserted.rows_affected() == 1 {
        Ok(())
    } else {
        Err(api_error_code(
            StatusCode::CONFLICT,
            "IDEMPOTENCY_CONFLICT",
            "Idempotency-Key was completed concurrently; retry the request to read its result",
        ))
    }
}

fn api_package_error(status: StatusCode, err: BinaryPackageNameError) -> Response {
    (
        status,
        Json(ApiPackageErrorBody {
            code: err.code(),
            error: err.message(),
            expected_pattern: RELEASE_PACKAGE_PATTERN,
            example: RELEASE_PACKAGE_EXAMPLE,
        }),
    )
        .into_response()
}

fn parse_optional_i64(value: &str) -> Result<Option<i64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<i64>()
        .map(Some)
        .map_err(|_| "versionCode 必须是整数".to_owned())
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

    fn token_id(&self) -> i64 {
        self.inner.token_id
    }

    fn allows_app(&self, app_id: i64) -> bool {
        self.inner.allows_app(app_id)
    }

    fn allows_unit(&self, unit_id: i64) -> bool {
        self.inner.allows_unit(unit_id)
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

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::sqlite::SqliteConnectOptions;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::{
        apps::{AppService, CreateAppInput, UploadBinaryArtifactInput},
        artifact_storage::{ArtifactObjectVerifier, ArtifactStorageError, VerifiedArtifactObject},
        auth::{AuthService, MemorySessionStore},
        deploy::{
            CommandResult, CommandRunner, CommandSpec, ComposeExecutor, DeployError,
            SystemdExecutor,
        },
        nodes::NodeService,
        runtimefs::RuntimeFs,
        tasks::TaskService,
    };

    use super::*;

    struct TestWebApp {
        router: Router,
        db: SqlitePool,
        auth: AuthService,
        apps: AppService,
        tasks: TaskService,
        platform: PlatformConfigService,
        _data_dir: TempDir,
    }

    struct WebTestCommandRunner;

    struct WebTestArtifactObjectVerifier;

    struct WebTestDeploymentExecutor;

    #[async_trait]
    impl DeploymentUnitExecutor for WebTestDeploymentExecutor {
        async fn execute(
            &self,
            context: crate::deployment_orchestrator::UnitExecutionContext,
        ) -> crate::deployment_orchestrator::UnitExecutionOutcome {
            crate::deployment_orchestrator::UnitExecutionOutcome::Success {
                summary: format!("{} deployed", context.item.unit_key),
            }
        }
    }

    #[async_trait]
    impl ArtifactObjectVerifier for WebTestArtifactObjectVerifier {
        async fn verify(
            &self,
            _config: &crate::artifact_storage::AliyunOssConfig,
            _object_key: &str,
        ) -> Result<VerifiedArtifactObject, ArtifactStorageError> {
            Ok(VerifiedArtifactObject {
                checksum_sha256: "a".repeat(64),
                size_bytes: 12,
                version_id: Some("web-test-version".to_owned()),
            })
        }

        async fn delete(
            &self,
            _config: &crate::artifact_storage::AliyunOssConfig,
            _object_key: &str,
            _version_id: Option<&str>,
        ) -> Result<(), ArtifactStorageError> {
            Ok(())
        }
    }

    #[async_trait]
    impl CommandRunner for WebTestCommandRunner {
        async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
            Ok(CommandResult {
                status_code: Some(0),
                stdout: format!("{} {}\n", spec.program, spec.args.join(" ")),
                stderr: String::new(),
            })
        }
    }

    async fn test_web_app() -> TestWebApp {
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
        let data_dir = tempfile::tempdir().expect("create test data dir");
        let settings = Settings {
            bind: "127.0.0.1:0".parse().expect("valid bind address"),
            database_url: "sqlite::memory:".to_owned(),
            data_dir: data_dir.path().to_path_buf(),
            cookie_secure: false,
            uploaded_binary_releases_to_keep: 4,
            command_timeout_secs: 120,
            config_active_key_id: "v1".to_owned(),
            config_master_keys: String::new(),
        };

        let tasks = TaskService::new(db.clone());
        let platform = PlatformConfigService::new(db.clone());
        let events = EventLogService::new(db.clone());
        let command_runner = Arc::new(WebTestCommandRunner);
        let nodes =
            NodeService::new_with_data_dir(db.clone(), command_runner.clone(), data_dir.path());
        let node_credentials = NodeCredentialService::new(db.clone(), data_dir.path());
        let apps = AppService::new_with_artifact_object_verifier(
            db.clone(),
            RuntimeFs::new(data_dir.path()),
            ComposeExecutor::new(command_runner.clone()),
            SystemdExecutor::new(command_runner.clone())
                .with_ssh_known_hosts_file(crate::deploy::ssh_known_hosts_file(data_dir.path())),
            tasks.clone(),
            platform.clone(),
            Arc::new(WebTestArtifactObjectVerifier),
        )
        .await
        .expect("create app service");
        let apps_for_test = apps.clone();
        let tasks_for_test = tasks.clone();
        let platform_for_test = platform.clone();
        TestWebApp {
            router: build_router(AppState::new(
                settings,
                db.clone(),
                AppStateServices {
                    auth,
                    nodes,
                    node_credentials,
                    apps,
                    tasks,
                    platform,
                    events,
                    application_config: None,
                    application_releases: ApplicationReleaseService::new(db.clone()),
                    deployment_orchestrator: DeploymentOrchestratorService::new(db.clone()),
                    deployment_console: DeploymentConsoleService::new(db.clone()),
                    deployment_executor: Some(Arc::new(WebTestDeploymentExecutor)),
                    deployment_logs: DeploymentLogService::new(db.clone()),
                    deployment_retention: DeploymentRetentionService::new(db.clone()),
                },
            )),
            auth: auth_for_test,
            db,
            apps: apps_for_test,
            tasks: tasks_for_test,
            platform: platform_for_test,
            _data_dir: data_dir,
        }
    }

    async fn test_app_with_auth() -> (Router, AuthService) {
        let app = test_web_app().await;
        (app.router, app.auth)
    }

    async fn test_app() -> Router {
        test_app_with_auth().await.0
    }

    async fn super_admin_api_token(auth: &AuthService, source: &str) -> String {
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
        auth.create_api_token(&login.session, source)
            .await
            .expect("create api token")
            .token
    }

    async fn enable_oss_storage(platform: &PlatformConfigService) {
        platform
            .update_config(
                UpdatePlatformConfigInput {
                    default_app_work_dir: "/opt/easy-deploy/apps/{app_key}".to_owned(),
                    default_node_work_dir: "/opt/easy-deploy/apps".to_owned(),
                    uploaded_binary_releases_to_keep: 4,
                    artifact_storage_provider: "aliyun_oss".to_owned(),
                    aliyun_oss_region: "oss-cn-hangzhou".to_owned(),
                    aliyun_oss_endpoint: "https://oss-cn-hangzhou.aliyuncs.com".to_owned(),
                    aliyun_oss_bucket: "easy-deploy-test".to_owned(),
                    aliyun_oss_object_prefix: "easy-deploy/releases".to_owned(),
                    aliyun_oss_access_key_id: "test-key".to_owned(),
                    aliyun_oss_access_key_secret: "test-secret".to_owned(),
                    aliyun_oss_upload_url_ttl_seconds: 900,
                    aliyun_oss_download_url_ttl_seconds: 600,
                },
                "test",
            )
            .await
            .expect("enable oss storage");
    }

    async fn create_binary_test_app(apps: &AppService, app_key: &str) -> i64 {
        apps.create_app(CreateAppInput {
            app_key: app_key.to_owned(),
            name: format!("{app_key} app"),
            description: String::new(),
            environment: "test".to_owned(),
            app_type: "binary".to_owned(),
            deploy_strategy: "rolling_stop_on_failure".to_owned(),
            release_source: "manual".to_owned(),
            auto_queue_release: true,
            work_dir: format!("/opt/easy-deploy/apps/{app_key}"),
            target_node_ids: vec![1],
            compose_content: String::new(),
            env_content: "RUST_LOG=info".to_owned(),
            deploy_scripts: DeployScriptSet::default(),
            health_check: Default::default(),
            binary_artifact_version: "v1.0.0".to_owned(),
            binary_artifact_path: format!(
                "/opt/easy-deploy/apps/{app_key}/releases/v1.0.0/{app_key}"
            ),
            binary_exec_args: "--port 8080".to_owned(),
            binary_service_user: "deploy".to_owned(),
            binary_unit_name: format!("easy-deploy-{app_key}.service"),
            binary_release_strategy: "restart".to_owned(),
            binary_active_slot: "blue".to_owned(),
            binary_base_port: 8080,
            binary_standby_port: 18080,
            binary_proxy_enabled: false,
            binary_proxy_kind: "none".to_owned(),
            binary_proxy_domain: String::new(),
            binary_proxy_config_path: String::new(),
        })
        .await
        .expect("create binary app")
    }

    async fn create_compose_test_app(apps: &AppService, app_key: &str) -> i64 {
        create_compose_test_app_with_mode(apps, app_key, true).await
    }

    async fn seed_deployable_application_release(db: &SqlitePool, app_id: i64) -> (i64, i64) {
        let environment_id: i64 = sqlx::query_scalar(
            "SELECT id FROM app_environments WHERE app_id = ?1 ORDER BY id LIMIT 1",
        )
        .bind(app_id)
        .fetch_one(db)
        .await
        .expect("load environment");
        sqlx::query("UPDATE app_environments SET status = 'ready' WHERE id = ?1")
            .bind(environment_id)
            .execute(db)
            .await
            .expect("ready environment");
        let unit_id: i64 = sqlx::query_scalar(
            "SELECT id FROM deployment_units WHERE app_id = ?1 ORDER BY id LIMIT 1",
        )
        .bind(app_id)
        .fetch_one(db)
        .await
        .expect("load unit");
        let config_revision_id = sqlx::query(
            "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, secret_ciphertext, config_hash) VALUES (?1, 100, '{}', '{}', '{}', 'web-deploy-config')",
        )
        .bind(app_id)
        .execute(db)
        .await
        .expect("insert config revision")
        .last_insert_rowid();
        let unit_release_id = sqlx::query(
            "INSERT INTO deployment_unit_releases(unit_id, version, version_code, package_name, package_path, checksum_sha256) VALUES (?1, '1.0.0', 100, 'default.tar.gz', '/tmp/default.tar.gz', 'unit-checksum')",
        )
        .bind(unit_id)
        .execute(db)
        .await
        .expect("insert unit release")
        .last_insert_rowid();
        let app_release_id = sqlx::query(
            "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '1.0.0', 100, 'received', 'openapi')",
        )
        .bind(app_id)
        .execute(db)
        .await
        .expect("insert app release")
        .last_insert_rowid();
        sqlx::query("INSERT INTO application_release_manifests(app_release_id, manifest_hash, manifest_json) VALUES (?1, ?2, '{}')")
            .bind(app_release_id)
            .bind(format!("web-deploy-manifest-{app_id}"))
            .execute(db)
            .await
            .expect("insert manifest");
        sqlx::query("INSERT INTO app_release_units(app_release_id, unit_id, unit_release_id, target_fingerprint) VALUES (?1, ?2, ?3, 'web-target')")
            .bind(app_release_id)
            .bind(unit_id)
            .bind(unit_release_id)
            .execute(db)
            .await
            .expect("insert release unit");
        sqlx::query("INSERT INTO app_release_environment_configs(app_release_id, environment_id, config_revision_id) VALUES (?1, ?2, ?3)")
            .bind(app_release_id)
            .bind(environment_id)
            .bind(config_revision_id)
            .execute(db)
            .await
            .expect("bind environment config");
        (environment_id, app_release_id)
    }

    async fn create_compose_test_app_with_mode(
        apps: &AppService,
        app_key: &str,
        auto_queue_release: bool,
    ) -> i64 {
        apps.create_app(CreateAppInput {
            app_key: app_key.to_owned(),
            name: format!("{app_key} app"),
            description: String::new(),
            environment: "test".to_owned(),
            app_type: "compose".to_owned(),
            deploy_strategy: "rolling_stop_on_failure".to_owned(),
            release_source: "package_upload".to_owned(),
            auto_queue_release,
            work_dir: format!("/opt/easy-deploy/apps/{app_key}"),
            target_node_ids: vec![1],
            compose_content: "services:\n  app:\n    image: nginx:alpine\n".to_owned(),
            env_content: "RUST_LOG=info".to_owned(),
            deploy_scripts: DeployScriptSet::default(),
            health_check: Default::default(),
            binary_artifact_version: String::new(),
            binary_artifact_path: String::new(),
            binary_exec_args: String::new(),
            binary_service_user: String::new(),
            binary_unit_name: String::new(),
            binary_release_strategy: "restart".to_owned(),
            binary_active_slot: "blue".to_owned(),
            binary_base_port: 8080,
            binary_standby_port: 18080,
            binary_proxy_enabled: false,
            binary_proxy_kind: "none".to_owned(),
            binary_proxy_domain: String::new(),
            binary_proxy_config_path: String::new(),
        })
        .await
        .expect("create compose app")
    }

    #[tokio::test]
    async fn apps_page_only_exposes_compose_creation() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/apps")
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("创建一个 Docker Compose 发布单元"));
        assert!(html.contains("name=\"app_type\" value=\"compose\""));
        assert!(html.contains("name=\"release_source\""));
        assert!(html.contains("value=\"package_upload\""));
        assert!(html.contains("目标版本"));
        assert!(html.contains("部署状态"));
        assert!(!html.contains("name=\"private_bucket\""));
        assert!(!html.contains("qfy-sc worker"));
        assert!(!html.contains("二进制直部署"));
        assert!(!html.contains("systemd 管理"));
    }

    #[tokio::test]
    async fn deployment_confirmation_creates_and_executes_environment_run() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "deploy-console-app").await;
        let (environment_id, app_release_id) =
            seed_deployable_application_release(&app.db, app_id).await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);
        let plan = DeploymentOrchestratorService::new(app.db.clone())
            .preview(environment_id, app_release_id, DeploymentMode::Normal)
            .await
            .expect("preview deployment");

        let preview_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/apps/{app_id}/deploy?environment_id={environment_id}&app_release_id={app_release_id}&mode=normal"
                    ))
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build preview request"),
            )
            .await
            .expect("send preview request");
        assert_eq!(preview_response.status(), StatusCode::OK);
        let preview_html = String::from_utf8_lossy(
            &to_bytes(preview_response.into_body(), usize::MAX)
                .await
                .expect("read preview body"),
        )
        .into_owned();
        assert!(preview_html.contains("确认部署"));
        assert!(preview_html.contains(&plan.plan_hash));

        let form = format!(
            "csrf_token={}&environment_id={environment_id}&app_release_id={app_release_id}&mode=normal&expected_plan_hash={}",
            login.session.csrf_token, plan.plan_hash
        );
        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/apps/{app_id}/deploy"))
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .expect("build deploy request"),
            )
            .await
            .expect("send deploy request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let mut status = String::new();
        for _ in 0..50 {
            status = sqlx::query_scalar(
                "SELECT status FROM environment_deployment_runs WHERE environment_id = ?1 ORDER BY id DESC LIMIT 1",
            )
            .bind(environment_id)
            .fetch_one(&app.db)
            .await
            .expect("load run status");
            if status == "success" {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(status, "success");
        let environment_status: (String, String) = sqlx::query_as(
            "SELECT runtime_status, last_deployment_status FROM app_environments WHERE id = ?1",
        )
        .bind(environment_id)
        .fetch_one(&app.db)
        .await
        .expect("load environment status");
        assert_eq!(
            environment_status,
            ("running".to_owned(), "success".to_owned())
        );
    }

    #[tokio::test]
    async fn deployment_cancel_route_requires_csrf_and_records_request() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "cancel-deployment-app").await;
        let (environment_id, app_release_id) =
            seed_deployable_application_release(&app.db, app_id).await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);
        let orchestrator = DeploymentOrchestratorService::new(app.db.clone());
        let preview = orchestrator
            .preview(environment_id, app_release_id, DeploymentMode::Normal)
            .await
            .expect("preview deployment");
        let run = orchestrator
            .create_run(CreateDeploymentRunInput {
                environment_id,
                app_release_id,
                mode: DeploymentMode::Normal,
                expected_plan_hash: preview.plan_hash,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create deployment run");

        let task_page = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/tasks/{}", run.task_id))
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build task page request"),
            )
            .await
            .expect("load task page");
        assert_eq!(task_page.status(), StatusCode::OK);
        let html = String::from_utf8_lossy(
            &to_bytes(task_page.into_body(), usize::MAX)
                .await
                .expect("read task page"),
        )
        .into_owned();
        assert!(html.contains("取消部署"));
        assert!(html.contains("当前单元结果未知，不会自动回滚"));

        let invalid = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/deployments/{}/cancel", run.deployment_run_id))
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("csrf_token=wrong"))
                    .expect("build invalid cancel request"),
            )
            .await
            .expect("send invalid cancel request");
        assert_eq!(invalid.status(), StatusCode::FORBIDDEN);
        let cancel_requested_at: Option<String> = sqlx::query_scalar(
            "SELECT cancel_requested_at FROM environment_deployment_runs WHERE id = ?1",
        )
        .bind(run.deployment_run_id)
        .fetch_one(&app.db)
        .await
        .expect("load cancellation state");
        assert!(cancel_requested_at.is_none());

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/deployments/{}/cancel", run.deployment_run_id))
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}",
                        login.session.csrf_token
                    )))
                    .expect("build cancel request"),
            )
            .await
            .expect("send cancel request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cancellation: (Option<String>, String, String) = sqlx::query_as(
            "SELECT cancel_requested_at, cancel_requested_by, status FROM environment_deployment_runs WHERE id = ?1",
        )
        .bind(run.deployment_run_id)
        .fetch_one(&app.db)
        .await
        .expect("load recorded cancellation");
        assert!(cancellation.0.is_some());
        assert_eq!(cancellation.1, "admin");
        assert_eq!(cancellation.2, "canceled");

        let legacy_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/tasks/{}/cancel", run.task_id))
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}",
                        login.session.csrf_token
                    )))
                    .expect("build legacy task cancel request"),
            )
            .await
            .expect("send legacy task cancel request");
        assert_eq!(legacy_response.status(), StatusCode::CONFLICT);
        let task_status: String =
            sqlx::query_scalar("SELECT status FROM operation_tasks WHERE id = ?1")
                .bind(run.task_id)
                .fetch_one(&app.db)
                .await
                .expect("load task status after legacy cancel route");
        assert_eq!(task_status, "canceled");
    }

    #[tokio::test]
    async fn deployment_reconciliation_requires_dedicated_permission_and_note() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "reconcile-deployment-app").await;
        let (environment_id, app_release_id) =
            seed_deployable_application_release(&app.db, app_id).await;
        let admin = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let admin_cookie = format!("ed_access={}", admin.tokens.access_token);
        let deployer_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'deployer'")
                .fetch_one(&app.db)
                .await
                .expect("load deployer role");
        app.auth
            .create_account(
                &admin.session,
                "deployer_user",
                "Deployer",
                "password123",
                &[deployer_role_id],
            )
            .await
            .expect("create deployer account");
        let deployer = app
            .auth
            .login(LoginInput {
                username: "deployer_user".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("login deployer");
        let deployer_cookie = format!("ed_access={}", deployer.tokens.access_token);
        let orchestrator = DeploymentOrchestratorService::new(app.db.clone());
        let preview = orchestrator
            .preview(environment_id, app_release_id, DeploymentMode::Force)
            .await
            .expect("preview deployment");
        let run = orchestrator
            .create_run(CreateDeploymentRunInput {
                environment_id,
                app_release_id,
                mode: DeploymentMode::Force,
                expected_plan_hash: preview.plan_hash,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create deployment run");
        sqlx::query(
            "UPDATE environment_deployment_runs SET status = 'running', started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
        )
        .bind(run.deployment_run_id)
        .execute(&app.db)
        .await
        .expect("mark run running");
        sqlx::query("UPDATE operation_tasks SET status = 'running' WHERE id = ?1")
            .bind(run.task_id)
            .execute(&app.db)
            .await
            .expect("mark task running");
        sqlx::query(
            "UPDATE deployment_unit_run_results SET status = 'running' WHERE deployment_run_id = ?1",
        )
        .bind(run.deployment_run_id)
        .execute(&app.db)
        .await
        .expect("mark unit running");
        let recovery = orchestrator
            .reconcile_interrupted_runs()
            .await
            .expect("reconcile interrupted run");
        assert_eq!(recovery.reconciling_runs, 1);

        let task_page = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/tasks/{}", run.task_id))
                    .header(header::COOKIE, &admin_cookie)
                    .body(Body::empty())
                    .expect("build task page request"),
            )
            .await
            .expect("load task page");
        let html = String::from_utf8_lossy(
            &to_bytes(task_page.into_body(), usize::MAX)
                .await
                .expect("read task page"),
        )
        .into_owned();
        assert!(html.contains("确认远端已停止"));
        assert!(html.contains("确认并释放环境锁"));

        let forbidden_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/deployments/{}/confirm-stopped",
                        run.deployment_run_id
                    ))
                    .header(header::COOKIE, &deployer_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&note=checked+all+nodes",
                        deployer.session.csrf_token
                    )))
                    .expect("build deployer reconciliation request"),
            )
            .await
            .expect("send deployer reconciliation request");
        assert_eq!(forbidden_response.status(), StatusCode::FORBIDDEN);

        let invalid_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/deployments/{}/confirm-stopped",
                        run.deployment_run_id
                    ))
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&note=",
                        admin.session.csrf_token
                    )))
                    .expect("build invalid reconciliation request"),
            )
            .await
            .expect("send invalid reconciliation request");
        assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/deployments/{}/confirm-stopped",
                        run.deployment_run_id
                    ))
                    .header(header::COOKIE, &admin_cookie)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&note=checked+all+target+nodes",
                        admin.session.csrf_token
                    )))
                    .expect("build reconciliation request"),
            )
            .await
            .expect("send reconciliation request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let reconciliation: (String, String, Option<String>) = sqlx::query_as(
            "SELECT status, reconciled_by, reconciled_at FROM environment_deployment_runs WHERE id = ?1",
        )
        .bind(run.deployment_run_id)
        .fetch_one(&app.db)
        .await
        .expect("load reconciliation result");
        assert_eq!(reconciliation.0, "canceled");
        assert_eq!(reconciliation.1, "admin");
        assert!(reconciliation.2.is_some());
    }

    #[tokio::test]
    async fn app_create_route_forces_compose_app_type() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);
        let form = format!(
            "csrf_token={}&app_key=forced-compose&name=Forced+Compose&description=&environment=test&app_type=binary&deploy_strategy=rolling_stop_on_failure&auto_queue_release=true&work_dir=/opt/easy-deploy/apps/forced-compose&compose_content=services%3A%0A++app%3A%0A++++image%3A+nginx%3Aalpine%0A&env_content=&target_node_ids=1",
            login.session.csrf_token
        );

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/apps")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .expect("build request"),
            )
            .await
            .expect("send request");
        let status = response.status();
        if status != StatusCode::SEE_OTHER {
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read failure body");
            panic!(
                "expected create app redirect, got {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }

        let apps = app.apps.list_apps().await.expect("list apps");
        let created = apps
            .into_iter()
            .find(|item| item.app_key == "forced-compose")
            .expect("created app");
        assert_eq!(created.app_type, "compose");
        assert_eq!(created.release_source, "package_upload");
    }

    #[tokio::test]
    async fn app_create_route_persists_template_scripts_and_health_check() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);
        let form = format!(
            "csrf_token={}&app_key=qfy-sc-test-backend&name=qfy-sc+测试+API&description=&environment=testing&deploy_strategy=rolling_stop_on_failure&release_source=package_upload&auto_queue_release=true&work_dir=/opt/easy-deploy/apps/qfy-sc-test-backend&compose_content=services%3A%0A++api%3A%0A++++image%3A+qfy-sc-test-api%3Alatest%0A&env_content=APP_ENV%3Dtesting%0A&deploy_script_pre_deploy=set+-eu%0Aecho+preflight%0A&deploy_script_deploy=docker+compose+up+-d%0A&deploy_script_post_deploy=.%2Foc-api+seed+core%0A&health_check_kind=http&health_endpoint=http%3A%2F%2F127.0.0.1%3A23710%2Fhealthz&health_timeout_secs=5&health_expected_status=200&target_node_ids=1",
            login.session.csrf_token
        );

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/apps")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form))
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        let created = app
            .apps
            .list_apps()
            .await
            .expect("list apps")
            .into_iter()
            .find(|item| item.app_key == "qfy-sc-test-backend")
            .expect("created app");
        let detail = app.apps.app_detail(created.id).await.expect("app detail");

        assert_eq!(detail.app.release_source, "package_upload");
        assert_eq!(detail.health_check.kind.as_str(), "http");
        assert_eq!(
            detail.health_check.endpoint,
            "http://127.0.0.1:23710/healthz"
        );
        assert!(detail.deploy_scripts.pre_deploy.contains("preflight"));
        assert!(
            detail
                .deploy_scripts
                .deploy
                .contains("docker compose up -d")
        );
        assert!(detail.deploy_scripts.post_deploy.contains("seed core"));
    }

    fn multipart_body(
        boundary: &str,
        file_field: &str,
        file_name: &str,
        file_content: &str,
        fields: &[(&str, &str)],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file_content.as_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
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
    async fn dashboard_host_metrics_returns_authenticated_snapshot() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/dashboard/host-metrics")
                    .header(header::COOKIE, &cookie_value)
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
            serde_json::from_slice(&body).expect("host metrics json response");

        assert!(payload["cpu"]["percent"].is_number());
        assert!(payload["memory"]["detail"].is_string());
        assert!(payload["disk"]["mount_point"].is_string());
        assert!(payload["disk_rate"]["detail"].is_string());
        assert!(payload["disk_rate"]["utilization_label"].is_string());
        assert!(payload["disk_rate"]["devices"].is_array());
        assert!(payload["disk_rate"]["processes"].is_array());
        assert!(payload["disk_rate"]["process_detail"].is_string());
        assert!(payload["network_rate"]["detail"].is_string());
    }

    #[tokio::test]
    async fn dashboard_host_metrics_redirects_revoked_session_to_login() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);
        app.auth
            .logout(&login.session)
            .await
            .expect("revoke session");

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .uri("/api/dashboard/host-metrics")
                    .header(header::COOKIE, cookie_value)
                    .header(header::ACCEPT, "application/json")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&"/login?notice=expired".parse().expect("valid location"))
        );
        assert_eq!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .count(),
            2
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

    #[test]
    fn background_fetches_redirect_top_level_when_login_is_required() {
        assert!(APP_JS.contains("const redirectToLoginIfNeeded = (response) =>"));
        assert_eq!(
            APP_JS
                .matches("if (redirectToLoginIfNeeded(response)) return;")
                .count(),
            APP_JS.matches("await fetch(").count(),
            "all authenticated background fetch paths must redirect the top-level page"
        );
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
        let paths = spec["paths"].as_object().expect("paths object");
        assert_eq!(paths.len(), 5);
        assert!(paths.contains_key("/api/v1/apps/{app_key}/units/{unit_key}/releases"));
        assert!(paths.contains_key("/api/v1/apps/{app_key}/releases"));
        assert!(paths.contains_key("/api/v1/services/{service_key}/packages/uploads"));
        assert!(
            paths.contains_key(
                "/api/v1/services/{service_key}/packages/uploads/{upload_id}/complete"
            )
        );
        assert!(paths.contains_key("/api/v1/services/{service_key}/packages"));
        assert_eq!(
            spec["paths"]["/api/v1/services/{service_key}/packages/uploads"]["post"]["operationId"],
            "createServicePackageUpload"
        );
        assert_eq!(
            spec["paths"]["/api/v1/apps/{app_key}/units/{unit_key}/releases"]["post"]["operationId"],
            "uploadDeploymentUnitRelease"
        );
        assert_eq!(
            spec["paths"]["/api/v1/apps/{app_key}/releases"]["post"]["operationId"],
            "createApplicationRelease"
        );
        assert_eq!(
            spec["paths"]["/api/v1/services/{service_key}/packages/uploads/{upload_id}/complete"]["post"]
                ["operationId"],
            "completeServicePackageUpload"
        );
        assert_eq!(
            spec["paths"]["/api/v1/services/{service_key}/packages"]["post"]["operationId"],
            "uploadServicePackageLegacy"
        );
        assert!(spec["paths"]["/api/v1/apps"].is_null());
        assert!(spec["paths"]["/api/v1/tasks"].is_null());
        assert!(spec["paths"]["/api/v1/services/{service_key}/deploy"].is_null());
        assert_eq!(
            spec["components"]["schemas"]["CreatePackageUploadRequest"]["properties"]["file_name"]
                ["type"],
            "string"
        );
        assert_eq!(
            spec["components"]["schemas"]["CompletePackageUploadRequest"]["properties"]["checksum_sha256"]
                ["type"],
            "string"
        );
        assert_eq!(
            spec["components"]["schemas"]["UploadServicePackageResponse"]["properties"]["data"]["properties"]
                ["task_id"]["type"][1],
            "null"
        );
        assert!(spec["components"]["schemas"]["CreateAppRequest"].is_null());
        assert!(spec["components"]["schemas"]["DeployAppRequest"].is_null());
        assert!(spec["components"]["schemas"]["TaskDetailResponse"].is_null());

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
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Easy Deploy OpenAPI"));
        assert!(html.contains("版本包与应用版本接口"));
        assert!(html.contains("/api/v1/apps/{app_key}/units/{unit_key}/releases"));
        assert!(html.contains("/api/v1/apps/{app_key}/releases"));
        assert!(html.contains("申请上传地址"));
        assert!(html.contains("完成登记"));
        assert!(html.contains("sha256sum"));
        assert!(html.contains("complete"));
        assert!(html.contains("/api/v1/services/{service_key}/packages/uploads"));
        assert!(html.contains("/api/v1/services/{service_key}/packages"));
        assert!(html.contains("{service_key}_version_{x_y_z}.tar.gz"));
        assert!(html.contains("file_name"));
        assert!(html.contains("checksum_sha256"));
        assert!(html.contains("source"));
        assert!(!html.contains("/api/v1/tasks"));
        assert!(!html.contains("/api/v1/services/{service_key}/deploy"));
        assert!(!html.contains("Git 源码发布"));
    }

    #[tokio::test]
    async fn api_v1_requires_bearer_token() {
        let response = test_app()
            .await
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages")
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
    async fn api_token_can_call_package_upload_api() {
        let app = test_web_app().await;
        create_compose_test_app(&app.apps, "orders-api-prod").await;
        let token = super_admin_api_token(&app.auth, "test-suite").await;
        let boundary = "easy-deploy-test-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "orders-api-prod-v1.2.3.tar.gz",
            "binary data",
            &[],
        );

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let listed = app.auth.list_api_tokens().await.expect("list api tokens");
        assert_eq!(listed[0].source, "test-suite");
        assert!(listed[0].last_used_at.is_some());
    }

    #[tokio::test]
    async fn scoped_token_uploads_unit_and_creates_idempotent_application_release() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app_with_mode(&app.apps, "multi-api", false).await;
        let unit_id: i64 = sqlx::query_scalar(
            "SELECT id FROM deployment_units WHERE app_id = ?1 AND unit_key = 'default'",
        )
        .bind(app_id)
        .fetch_one(&app.db)
        .await
        .expect("load default unit");
        let environment_id: i64 = sqlx::query_scalar(
            "SELECT id FROM app_environments WHERE app_id = ?1 ORDER BY id LIMIT 1",
        )
        .bind(app_id)
        .fetch_one(&app.db)
        .await
        .expect("load environment");
        let config_revision_id = sqlx::query(
            "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, config_hash) VALUES (?1, 100, '{}', '{}', 'multi-api-config')",
        )
        .bind(app_id)
        .execute(&app.db)
        .await
        .expect("insert config revision")
        .last_insert_rowid();
        let token = super_admin_api_token(&app.auth, "multi-unit-test").await;
        let token_id: i64 = sqlx::query_scalar(
            "SELECT id FROM api_tokens WHERE source = 'multi-unit-test' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&app.db)
        .await
        .expect("load token");
        sqlx::query(
            "UPDATE api_tokens SET app_scope_json = ?2, unit_scope_json = ?3, max_concurrent_requests = 10 WHERE id = ?1",
        )
        .bind(token_id)
        .bind(serde_json::json!([app_id]).to_string())
        .bind(serde_json::json!([unit_id]).to_string())
        .execute(&app.db)
        .await
        .expect("scope token");
        let boundary = "unit-release-boundary";
        let upload_body = multipart_body(
            boundary,
            "package_file",
            "multi-api-default-1.0.0.tar.gz",
            "unit package",
            &[("artifact_version", "1.0.0"), ("source", "ci")],
        );
        let upload_request = || {
            Request::builder()
                .method("POST")
                .uri("/api/v1/apps/multi-api/units/default/releases")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("idempotency-key", "unit-default-1.0.0")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(upload_body.clone()))
                .expect("build upload request")
        };

        let response = app
            .router
            .clone()
            .oneshot(upload_request())
            .await
            .expect("upload unit");
        assert_eq!(response.status(), StatusCode::CREATED);
        let upload: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read upload response"),
        )
        .expect("parse upload response");
        let unit_release_id = upload["data"]["unit_release_id"]
            .as_i64()
            .expect("unit release id");
        let replay = app
            .router
            .clone()
            .oneshot(upload_request())
            .await
            .expect("replay upload");
        assert_eq!(replay.status(), StatusCode::CREATED);

        let release_payload = serde_json::json!({
            "version": "1.0.0",
            "unit_changes": [{
                "unit_id": unit_id,
                "unit_release_id": unit_release_id,
                "desired_status": "active"
            }],
            "environment_configs": [{
                "environment_id": environment_id,
                "config_revision_id": config_revision_id
            }]
        });
        let release_request = |payload: &serde_json::Value| {
            Request::builder()
                .method("POST")
                .uri("/api/v1/apps/multi-api/releases")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("idempotency-key", "app-release-1.0.0")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.to_string()))
                .expect("build app release request")
        };
        let response = app
            .router
            .clone()
            .oneshot(release_request(&release_payload))
            .await
            .expect("create application release");
        assert_eq!(response.status(), StatusCode::CREATED);
        let release: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read release response"),
        )
        .expect("parse release response");
        assert_eq!(release["data"]["versionCode"], 100);
        assert_eq!(release["data"]["deployment_started"], false);
        let replay = app
            .router
            .clone()
            .oneshot(release_request(&release_payload))
            .await
            .expect("replay application release");
        assert_eq!(replay.status(), StatusCode::CREATED);

        let mut changed_payload = release_payload;
        changed_payload["version"] = serde_json::Value::String("1.0.1".to_owned());
        let conflict = app
            .router
            .clone()
            .oneshot(release_request(&changed_payload))
            .await
            .expect("send idempotency conflict");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM environment_deployment_runs")
            .fetch_one(&app.db)
            .await
            .expect("count deployment runs");
        assert_eq!(run_count, 0);
    }

    #[tokio::test]
    async fn new_openapi_enforces_token_scope_expiry_and_rate_limit() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app_with_mode(&app.apps, "scope-api", false).await;
        let token = super_admin_api_token(&app.auth, "scope-policy-test").await;
        let token_id: i64 = sqlx::query_scalar(
            "SELECT id FROM api_tokens WHERE source = 'scope-policy-test' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&app.db)
        .await
        .expect("load token");
        let boundary = "scope-policy-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "scope-api.tar.gz",
            "package",
            &[("artifact_version", "1.0.0")],
        );
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/api/v1/apps/scope-api/units/default/releases")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("idempotency-key", "scope-policy")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body.clone()))
                .expect("build request")
        };

        let denied = app
            .router
            .clone()
            .oneshot(request())
            .await
            .expect("send scope denied request");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let denied_body = to_bytes(denied.into_body(), usize::MAX)
            .await
            .expect("read denied body");
        assert!(String::from_utf8_lossy(&denied_body).contains("RESOURCE_SCOPE_DENIED"));

        sqlx::query("UPDATE api_tokens SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1")
            .bind(token_id)
            .execute(&app.db)
            .await
            .expect("expire token");
        let expired = app
            .router
            .clone()
            .oneshot(request())
            .await
            .expect("send expired request");
        assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);

        sqlx::query(
            "UPDATE api_tokens SET expires_at = NULL, app_scope_json = ?2, unit_scope_json = '[]', rate_limit_per_minute = 1, rate_window_started_at = '', rate_window_count = 0, active_request_count = 0 WHERE id = ?1",
        )
        .bind(token_id)
        .bind(serde_json::json!([app_id]).to_string())
        .execute(&app.db)
        .await
        .expect("configure rate policy");
        let first = app
            .router
            .clone()
            .oneshot(request())
            .await
            .expect("send first rate request");
        assert_eq!(first.status(), StatusCode::FORBIDDEN);
        let limited = app
            .router
            .clone()
            .oneshot(request())
            .await
            .expect("send rate limited request");
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);

        sqlx::query(
            "UPDATE api_tokens SET rate_limit_per_minute = 100, rate_window_started_at = '', rate_window_count = 0, max_concurrent_requests = 1, active_request_count = 0 WHERE id = ?1",
        )
        .bind(token_id)
        .execute(&app.db)
        .await
        .expect("configure concurrency policy");
        let permit = app
            .auth
            .authenticate_api_token(&token, "127.0.0.1")
            .await
            .expect("acquire first token request");
        let concurrent = app
            .auth
            .authenticate_api_token(&token, "127.0.0.1")
            .await
            .expect_err("second concurrent request must fail");
        assert!(matches!(concurrent, crate::auth::AuthError::RateLimited(_)));
        drop(permit);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        app.auth
            .authenticate_api_token(&token, "127.0.0.1")
            .await
            .expect("permit is released after request drop");
    }

    #[tokio::test]
    async fn api_token_can_create_and_complete_oss_package_upload() {
        let app = test_web_app().await;
        create_compose_test_app_with_mode(&app.apps, "orders-api-prod", false).await;
        enable_oss_storage(&app.platform).await;
        let token = super_admin_api_token(&app.auth, "oss-upload-test").await;

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages/uploads")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"file_name":"orders-api-prod_version_1_2_3.tar.gz","source":"ai-agent"}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("create upload json response");
        assert_eq!(payload["data"]["release_version"], "v1.2.3");
        assert_eq!(payload["data"]["versionCode"], 1002003);
        assert_eq!(payload["data"]["upload"]["method"], "PUT");
        assert_eq!(
            payload["data"]["upload"]["headers"]["Content-Type"],
            "application/octet-stream"
        );
        assert_eq!(
            payload["data"]["upload"]["headers"]["x-oss-forbid-overwrite"],
            "true"
        );
        assert!(
            payload["data"]["upload"]["url"]
                .as_str()
                .expect("upload url")
                .contains("easy-deploy-test.oss-cn-hangzhou.aliyuncs.com")
        );
        let upload_id = payload["data"]["upload_id"]
            .as_str()
            .expect("upload id")
            .to_owned();
        assert!(
            payload["data"]["object_key"]
                .as_str()
                .expect("object key")
                .contains(&format!("/uploads/{upload_id}/"))
        );

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/v1/services/orders-api-prod/packages/uploads/{upload_id}/complete"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"checksum_sha256":"{}","size_bytes":12,"source":"ai-agent"}}"#,
                        "a".repeat(64)
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("complete upload json response");
        assert_eq!(payload["data"]["release_version"], "v1.2.3");
        assert_eq!(payload["data"]["queued"], false);

        let releases = app.apps.list_app_releases().await.expect("list releases");
        let release = releases
            .into_iter()
            .find(|release| release.version == "v1.2.3")
            .expect("registered oss release");
        assert_eq!(release.storage_provider, "aliyun_oss");
        assert_eq!(release.storage_bucket, "easy-deploy-test");
        assert!(
            release
                .storage_object_key
                .contains("orders-api-prod/v1.2.3")
        );
        assert_eq!(release.checksum_sha256, "a".repeat(64));
        assert_eq!(release.size_bytes, 12);
    }

    #[tokio::test]
    async fn api_token_create_redirect_prevents_refresh_duplicate() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api-tokens")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&source=refresh-test",
                        login.session.csrf_token
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("redirect location")
            .to_owned();
        assert!(location.starts_with("/admin/api-tokens?created="));
        assert_eq!(app.auth.list_api_tokens().await.unwrap().len(), 1);

        for _ in 0..2 {
            let response = app
                .router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&location)
                        .header(header::COOKIE, &cookie_value)
                        .body(Body::empty())
                        .expect("build request"),
                )
                .await
                .expect("send request");
            assert_eq!(response.status(), StatusCode::OK);
        }

        let tokens = app.auth.list_api_tokens().await.expect("list tokens");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].source, "refresh-test");
    }

    #[tokio::test]
    async fn api_token_delete_only_removes_revoked_tokens() {
        let app = test_web_app().await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let created = app
            .auth
            .create_api_token(&login.session, "delete-test")
            .await
            .expect("create token");

        let active_delete = app
            .auth
            .delete_revoked_api_token(&login.session, created.id)
            .await;
        assert!(active_delete.is_err());
        assert_eq!(app.auth.list_api_tokens().await.unwrap().len(), 1);

        app.auth
            .revoke_api_token(&login.session, created.id)
            .await
            .expect("revoke token");
        app.auth
            .delete_revoked_api_token(&login.session, created.id)
            .await
            .expect("delete revoked token");

        assert!(app.auth.list_api_tokens().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_v1_package_upload_requires_artifact_permission() {
        let app = test_web_app().await;
        create_compose_test_app(&app.apps, "orders-api-prod").await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let viewer_role_id = app
            .auth
            .list_role_options()
            .await
            .expect("list roles")
            .into_iter()
            .find(|role| role.role_code == "viewer")
            .expect("viewer role")
            .id;
        app.auth
            .create_account(
                &login.session,
                "viewer",
                "Viewer",
                "password123",
                &[viewer_role_id],
            )
            .await
            .expect("create viewer");
        let viewer_login = app
            .auth
            .login(LoginInput {
                username: "viewer".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("viewer login");
        let token = app
            .auth
            .create_api_token(&viewer_login.session, "viewer-upload-denied")
            .await
            .expect("create viewer api token");
        let boundary = "easy-deploy-test-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "orders-api-prod_version_1_2_3",
            "binary data",
            &[],
        );

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages")
                    .header(header::AUTHORIZATION, format!("Bearer {}", token.token))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn api_v1_package_upload_rejects_revoked_token() {
        let app = test_web_app().await;
        create_compose_test_app(&app.apps, "orders-api-prod").await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let created = app
            .auth
            .create_api_token(&login.session, "revoked-upload")
            .await
            .expect("create api token");
        app.auth
            .revoke_api_token(&login.session, created.id)
            .await
            .expect("revoke api token");
        let boundary = "easy-deploy-test-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "orders-api-prod_version_1_2_3",
            "binary data",
            &[],
        );

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages")
                    .header(header::AUTHORIZATION, format!("Bearer {}", created.token))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("api error json response");
        assert!(
            payload["error"]
                .as_str()
                .unwrap_or_default()
                .contains("已吊销")
        );
        assert!(app.apps.list_app_releases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_v1_package_upload_rejects_manual_release_source_app() {
        let app = test_web_app().await;
        app.apps
            .create_app(CreateAppInput {
                app_key: "orders-manual".to_owned(),
                name: "orders manual".to_owned(),
                description: String::new(),
                environment: "test".to_owned(),
                app_type: "compose".to_owned(),
                deploy_strategy: "rolling_stop_on_failure".to_owned(),
                release_source: "manual".to_owned(),
                auto_queue_release: false,
                work_dir: "/opt/easy-deploy/apps/orders-manual".to_owned(),
                target_node_ids: vec![1],
                compose_content: "services:\n  app:\n    image: nginx:alpine\n".to_owned(),
                env_content: String::new(),
                deploy_scripts: DeployScriptSet::default(),
                health_check: Default::default(),
                binary_artifact_version: String::new(),
                binary_artifact_path: String::new(),
                binary_exec_args: String::new(),
                binary_service_user: String::new(),
                binary_unit_name: String::new(),
                binary_release_strategy: "restart".to_owned(),
                binary_active_slot: "blue".to_owned(),
                binary_base_port: 8080,
                binary_standby_port: 18080,
                binary_proxy_enabled: false,
                binary_proxy_kind: "none".to_owned(),
                binary_proxy_domain: String::new(),
                binary_proxy_config_path: String::new(),
            })
            .await
            .expect("create manual release source app");
        let token = super_admin_api_token(&app.auth, "manual-upload-denied").await;
        let boundary = "easy-deploy-test-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "orders-manual_version_1_2_3",
            "binary data",
            &[],
        );

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-manual/packages")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("api error json response");
        assert!(
            payload["error"]
                .as_str()
                .unwrap_or_default()
                .contains("版本包发布模式")
        );
        assert!(app.apps.list_app_releases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn api_v1_control_endpoints_are_not_routed() {
        let app = test_web_app().await;
        let token = super_admin_api_token(&app.auth, "control-api-removed").await;

        let cases = [
            ("GET", "/api/v1/apps"),
            ("POST", "/api/v1/apps"),
            ("GET", "/api/v1/services/orders-api-prod/app"),
            ("PUT", "/api/v1/services/orders-api-prod/config"),
            ("POST", "/api/v1/services/orders-api-prod/deploy"),
            ("GET", "/api/v1/tasks/1"),
        ];

        for (method, uri) in cases {
            let response = app
                .router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .expect("build request"),
                )
                .await
                .expect("send request");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }
    #[tokio::test]
    async fn api_v1_package_upload_rejects_invalid_file_name() {
        let app = test_web_app().await;
        create_compose_test_app(&app.apps, "orders-api-prod").await;
        let token = super_admin_api_token(&app.auth, "package-test").await;
        let boundary = "easy-deploy-test-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "orders-api-prod-v1.2.3.tar.gz",
            "binary data",
            &[],
        );

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("package error json response");
        assert_eq!(payload["code"], "INVALID_PACKAGE_VERSION_NAME");
        assert_eq!(payload["expected_pattern"], RELEASE_PACKAGE_PATTERN);
        assert_eq!(payload["example"], RELEASE_PACKAGE_EXAMPLE);
    }

    #[tokio::test]
    async fn api_v1_package_upload_records_release_and_config_revision() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "orders-api-prod").await;
        let token = super_admin_api_token(&app.auth, "package-test").await;
        let boundary = "easy-deploy-test-boundary";
        let body = multipart_body(
            boundary,
            "package_file",
            "orders-api-prod_version_1_2_3",
            "orders binary v1.2.3",
            &[
                ("source", "local-script"),
                ("release_version", "v1.2.3"),
                ("versionCode", "1002003"),
                ("publishedAt", "2026-06-09T10:00:00Z"),
            ],
        );

        let response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/services/orders-api-prod/packages")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        assert_eq!(status, StatusCode::OK);
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("package upload json response");
        assert_eq!(payload["data"]["app_id"], app_id);
        assert_eq!(payload["data"]["service_key"], "orders-api-prod");
        assert_eq!(payload["data"]["release_version"], "v1.2.3");
        assert_eq!(payload["data"]["versionCode"], 1002003);
        assert_eq!(payload["data"]["publishedAt"], "2026-06-09T10:00:00Z");
        assert_eq!(payload["data"]["queued"], true);
        assert!(payload["data"]["release_id"].as_i64().unwrap_or_default() > 0);
        assert!(payload["data"]["queue_id"].as_i64().unwrap_or_default() > 0);
        assert!(
            payload["data"]["config_revision_no"]
                .as_i64()
                .unwrap_or_default()
                > 0
        );
        assert_eq!(payload["data"]["task_id"], serde_json::Value::Null);
        let releases = app
            .apps
            .list_app_releases()
            .await
            .expect("list app releases");
        let release = releases
            .into_iter()
            .find(|item| item.app_id == app_id && item.version == "v1.2.3")
            .expect("release item");
        assert_eq!(release.id, payload["data"]["release_id"]);
        assert!(
            matches!(
                release.status.as_str(),
                "queued" | "deploying" | "deployed" | "failed"
            ),
            "unexpected release status: {}",
            release.status
        );
        let queue_items = app
            .apps
            .list_app_release_queue()
            .await
            .expect("list release queue");
        let queue_item = queue_items
            .into_iter()
            .find(|item| item.app_id == app_id && item.version == "v1.2.3")
            .expect("queue item");
        assert_eq!(queue_item.id, payload["data"]["queue_id"]);
        assert_eq!(queue_item.release_id, release.id);
        assert!(
            matches!(
                queue_item.status.as_str(),
                "queued" | "running" | "success" | "failed"
            ),
            "unexpected queue status: {}",
            queue_item.status
        );
    }

    #[tokio::test]
    async fn app_detail_does_not_expose_object_storage_env_controls() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "orders-api-prod").await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let detail_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/apps/{app_id}"))
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(detail_response.status(), StatusCode::OK);
        let body = to_bytes(detail_response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body);
        assert!(!html.contains("name=\"private_bucket\""));
        assert!(!html.contains("name=\"application_key\""));
        assert!(!html.contains("ALIYUN_OSS_ACCESS_KEY_ID"));

        let settings_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(settings_response.status(), StatusCode::OK);
        let body = to_bytes(settings_response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("name=\"artifact_storage_provider\""));
        assert!(html.contains("name=\"aliyun_oss_bucket\""));
    }

    #[tokio::test]
    async fn app_detail_hides_legacy_binary_release_controls() {
        let app = test_web_app().await;
        let app_id = create_binary_test_app(&app.apps, "orders-api-prod").await;
        app.apps
            .upload_binary_artifact(UploadBinaryArtifactInput {
                app_id,
                artifact_version: "v1.2.3".to_owned(),
                version_code: None,
                published_at: "2026-06-09T10:00:00Z".to_owned(),
                file_name: "orders-api-prod".to_owned(),
                bytes: b"orders binary v1.2.3".to_vec(),
                entry_file: String::new(),
                source: "openapi-test".to_owned(),
            })
            .await
            .expect("upload binary release");
        let artifact_id = app
            .apps
            .app_detail(app_id)
            .await
            .expect("app detail")
            .binary_releases
            .into_iter()
            .find(|release| release.version == "v1.2.3")
            .expect("uploaded release")
            .id;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let detail_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/apps/{app_id}"))
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(detail_response.status(), StatusCode::OK);
        let body = to_bytes(detail_response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("运行配置"));
        assert!(html.contains("部署控制台"));
        assert!(html.contains("应用部署历史"));
        assert!(!html.contains("versionCode 1002003"));
        assert!(!html.contains(&format!("/apps/{app_id}/binary/upload")));
        assert!(!html.contains("二进制配置"));
        assert!(!html.contains("systemd 操作"));
        assert!(!html.contains("发布时间 2026-06-09T10:00:00Z"));
        assert!(artifact_id > 0);
    }

    #[tokio::test]
    async fn artifacts_page_uploads_release_package_for_selected_app() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "orders-api-prod").await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let page_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/artifacts")
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(page_response.status(), StatusCode::OK);
        let body = to_bytes(page_response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("action=\"/artifacts/upload\""));
        assert!(html.contains("name=\"app_id\""));
        assert!(html.contains("name=\"artifact_file\""));
        assert!(html.contains("orders-api-prod"));

        let app_id_field = app_id.to_string();
        let boundary = "easy-deploy-page-upload-boundary";
        let body = multipart_body(
            boundary,
            "artifact_file",
            "orders-api-prod_version_1_3_0",
            "orders binary v1.3.0",
            &[
                ("csrf_token", login.session.csrf_token.as_str()),
                ("app_id", app_id_field.as_str()),
                ("artifact_version", "v1.3.0"),
                ("versionCode", "1003000"),
                ("publishedAt", "2026-06-09T11:00:00Z"),
                ("entry_file", ""),
            ],
        );

        let upload_response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/upload")
                    .header(header::COOKIE, &cookie_value)
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send upload request");
        assert_eq!(upload_response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            upload_response.headers().get(header::LOCATION),
            Some(&"/artifacts?source=upload".parse().expect("valid header"))
        );

        let release = app
            .apps
            .list_app_releases()
            .await
            .expect("list releases")
            .into_iter()
            .find(|release| release.app_id == app_id && release.version == "v1.3.0")
            .expect("uploaded release");
        assert_eq!(release.version_code, 1_003_000);
        assert_eq!(
            artifact_metadata_value(&release.metadata, "source"),
            "package_upload"
        );
        assert_eq!(
            artifact_metadata_value(&release.metadata, "source_detail"),
            "upload"
        );
    }

    #[tokio::test]
    async fn artifacts_page_upload_requires_valid_csrf_without_creating_release() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app(&app.apps, "orders-api-prod").await;
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);
        let app_id_field = app_id.to_string();
        let boundary = "easy-deploy-page-upload-boundary";
        let body = multipart_body(
            boundary,
            "artifact_file",
            "orders-api-prod_version_1_4_0",
            "orders binary v1.4.0",
            &[
                ("csrf_token", "wrong-token"),
                ("app_id", app_id_field.as_str()),
                ("artifact_version", "v1.4.0"),
                ("versionCode", "1004000"),
                ("publishedAt", "2026-06-09T12:00:00Z"),
                ("entry_file", ""),
            ],
        );

        let upload_response = app
            .router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/upload")
                    .header(header::COOKIE, &cookie_value)
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("build request"),
            )
            .await
            .expect("send upload request");

        assert_eq!(upload_response.status(), StatusCode::FORBIDDEN);
        assert!(app.apps.list_app_releases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn manual_publish_mode_keeps_uploaded_release_received() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app_with_mode(&app.apps, "orders-api-manual", false).await;

        let result = app
            .apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.3".to_owned(),
                version_code: Some(1_002_003),
                published_at: "2026-06-09T10:00:00Z".to_owned(),
                file_name: "orders-api-manual".to_owned(),
                bytes: b"manual release".to_vec(),
                entry_file: String::new(),
                source: "local-script".to_owned(),
            })
            .await
            .expect("upload release package");

        assert!(!result.queued);
        let releases = app.apps.list_app_releases().await.expect("list releases");
        let release = releases
            .into_iter()
            .find(|item| item.id == result.release_id)
            .expect("release");
        assert_eq!(release.status, "received");
        assert_eq!(release.source, "openapi");
        assert_eq!(
            artifact_metadata_value(&release.metadata, "source"),
            "package_upload"
        );
        assert_eq!(
            artifact_metadata_value(&release.metadata, "source_detail"),
            "local-script"
        );
        let queue = app.apps.list_app_release_queue().await.expect("list queue");
        assert!(
            queue
                .into_iter()
                .all(|item| item.release_id != result.release_id)
        );
    }

    #[tokio::test]
    async fn schedule_release_accepts_datetime_local_value() {
        let app = test_web_app().await;
        let app_id =
            create_compose_test_app_with_mode(&app.apps, "orders-api-schedule", false).await;
        let result = app
            .apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.3".to_owned(),
                version_code: Some(1_002_003),
                published_at: "2026-06-09T10:00:00Z".to_owned(),
                file_name: "orders-api-schedule".to_owned(),
                bytes: b"scheduled release".to_vec(),
                entry_file: String::new(),
                source: "upload".to_owned(),
            })
            .await
            .expect("upload release package");

        let scheduled = app
            .apps
            .schedule_release_publish(result.release_id, "2026-06-23T15:30")
            .await
            .expect("schedule release");

        assert_eq!(scheduled, "2026-06-23T07:30:00Z");
        let releases = app.apps.list_app_releases().await.expect("list releases");
        let release = releases
            .into_iter()
            .find(|item| item.id == result.release_id)
            .expect("release");
        assert_eq!(release.status, "queued");
        assert_eq!(
            release.scheduled_publish_at.as_deref(),
            Some("2026-06-23T07:30:00Z")
        );
    }

    #[tokio::test]
    async fn artifacts_page_filters_and_formats_release_center_rows() {
        let app = test_web_app().await;
        let app_id = create_compose_test_app_with_mode(&app.apps, "orders-api-center", false).await;
        app.apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.3".to_owned(),
                version_code: Some(1_002_003),
                published_at: "2026-06-09T10:00:00Z".to_owned(),
                file_name: "orders-api-center".to_owned(),
                bytes: b"release center".to_vec(),
                entry_file: String::new(),
                source: "local-script".to_owned(),
            })
            .await
            .expect("upload release package");
        let release = app
            .apps
            .list_app_releases()
            .await
            .expect("list releases")
            .into_iter()
            .find(|item| item.app_id == app_id && item.version == "v1.2.3")
            .expect("release");
        app.apps
            .schedule_release_publish(release.id, "2026-06-23T15:30")
            .await
            .expect("schedule release");

        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/artifacts?status=queued&source=openapi")
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("OpenAPI"));
        assert!(html.contains("2026-06-09 18:00:00"));
        assert!(html.contains("计划 2026-06-23 15:30:00"));
        assert!(html.contains("action=\"/artifacts/schedule/cancel\""));
    }

    #[tokio::test]
    async fn artifacts_page_blocks_publish_while_same_app_deployment_is_running() {
        let app = test_web_app().await;
        let app_id =
            create_compose_test_app_with_mode(&app.apps, "orders-publish-guard", false).await;
        let release = app
            .apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.3".to_owned(),
                version_code: Some(1_002_003),
                published_at: "2026-06-09T10:00:00Z".to_owned(),
                file_name: "orders-publish-guard".to_owned(),
                bytes: b"release blocked by active deployment".to_vec(),
                entry_file: String::new(),
                source: "route-test".to_owned(),
            })
            .await
            .expect("upload release");
        let task_id = app
            .tasks
            .create_task(crate::tasks::CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "正在部署旧版本".to_owned(),
                app_id: Some(app_id),
                release_id: None,
                node_id: None,
                created_by: "test".to_owned(),
            })
            .await
            .expect("create deployment task");
        app.tasks
            .mark_running(task_id, "docker compose up", "deploy")
            .await
            .expect("mark deployment running");
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let page = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/artifacts")
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build page request"),
            )
            .await
            .expect("load artifacts page");
        assert_eq!(page.status(), StatusCode::OK);
        let page_body = to_bytes(page.into_body(), usize::MAX)
            .await
            .expect("read page body");
        let html = String::from_utf8_lossy(&page_body);
        assert!(html.contains("当前应用正在部署流程中"));
        assert!(!html.contains("action=\"/artifacts/publish\""));

        let publish = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/publish")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&release_id={}",
                        login.session.csrf_token, release.release_id
                    )))
                    .expect("build publish request"),
            )
            .await
            .expect("submit publish request");
        assert_eq!(publish.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            publish.headers().get(header::LOCATION),
            Some(
                &"/artifacts?notice=app-deploying"
                    .parse()
                    .expect("valid location")
            )
        );
        assert!(app.apps.list_app_release_queue().await.unwrap().is_empty());

        let notice_page = app
            .router
            .oneshot(
                Request::builder()
                    .uri("/artifacts?notice=app-deploying")
                    .header(header::COOKIE, &cookie_value)
                    .body(Body::empty())
                    .expect("build notice request"),
            )
            .await
            .expect("load notice page");
        let notice_body = to_bytes(notice_page.into_body(), usize::MAX)
            .await
            .expect("read notice body");
        assert!(String::from_utf8_lossy(&notice_body).contains(APP_DEPLOYMENT_IN_PROGRESS_MESSAGE));
    }

    #[tokio::test]
    async fn artifact_publish_schedule_and_queue_cancel_routes_update_release_state() {
        let app = test_web_app().await;
        let app_id =
            create_compose_test_app_with_mode(&app.apps, "orders-api-controls", false).await;
        let first_release = app
            .apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.3".to_owned(),
                version_code: Some(1_002_003),
                published_at: "2026-06-09T10:00:00Z".to_owned(),
                file_name: "orders-api-controls".to_owned(),
                bytes: b"release controls v1.2.3".to_vec(),
                entry_file: String::new(),
                source: "route-test".to_owned(),
            })
            .await
            .expect("upload first release");
        let second_release = app
            .apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.4".to_owned(),
                version_code: Some(1_002_004),
                published_at: "2026-06-10T10:00:00Z".to_owned(),
                file_name: "orders-api-controls".to_owned(),
                bytes: b"release controls v1.2.4".to_vec(),
                entry_file: String::new(),
                source: "route-test".to_owned(),
            })
            .await
            .expect("upload second release");
        let login = app
            .auth
            .bootstrap_init(LoginInput {
                username: "admin".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "test".to_owned(),
            })
            .await
            .expect("bootstrap admin");
        let cookie_value = format!("ed_access={}", login.tokens.access_token);

        let forbidden_schedule = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/schedule")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token=bad-token&release_id={}&scheduled_publish_at=2030-01-02T03%3A04%3A05Z",
                        first_release.release_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send forbidden schedule request");
        assert_eq!(forbidden_schedule.status(), StatusCode::FORBIDDEN);
        assert!(app.apps.list_app_release_queue().await.unwrap().is_empty());

        let schedule_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/schedule")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&release_id={}&scheduled_publish_at=2030-01-02T03%3A04%3A05Z",
                        login.session.csrf_token, first_release.release_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send schedule request");
        assert_eq!(schedule_response.status(), StatusCode::SEE_OTHER);

        let scheduled_queue = app
            .apps
            .list_app_release_queue()
            .await
            .expect("list scheduled queue")
            .into_iter()
            .find(|item| item.release_id == first_release.release_id)
            .expect("scheduled queue item");
        assert_eq!(scheduled_queue.status, "scheduled");
        assert_eq!(
            scheduled_queue.scheduled_publish_at.as_deref(),
            Some("2030-01-02T03:04:05Z")
        );

        let forbidden_cancel_schedule = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/schedule/cancel")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token=bad-token&release_id={}",
                        first_release.release_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send forbidden schedule cancel request");
        assert_eq!(forbidden_cancel_schedule.status(), StatusCode::FORBIDDEN);

        let cancel_schedule_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/schedule/cancel")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&release_id={}",
                        login.session.csrf_token, first_release.release_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send schedule cancel request");
        assert_eq!(cancel_schedule_response.status(), StatusCode::SEE_OTHER);
        let first_release_after_cancel = app
            .apps
            .list_app_releases()
            .await
            .expect("list releases after schedule cancel")
            .into_iter()
            .find(|release| release.id == first_release.release_id)
            .expect("first release after schedule cancel");
        assert_eq!(first_release_after_cancel.status, "received");

        app.apps
            .schedule_release_publish(first_release.release_id, "2030-01-02T03:04:05Z")
            .await
            .expect("schedule first release again");
        let queue_id = app
            .apps
            .list_app_release_queue()
            .await
            .expect("list queue before generic cancel")
            .into_iter()
            .find(|item| item.release_id == first_release.release_id && item.status == "scheduled")
            .expect("queue to cancel")
            .id;
        let forbidden_queue_cancel = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/queue/cancel")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token=bad-token&queue_id={queue_id}"
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send forbidden queue cancel request");
        assert_eq!(forbidden_queue_cancel.status(), StatusCode::FORBIDDEN);

        let queue_cancel_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/queue/cancel")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&queue_id={}",
                        login.session.csrf_token, queue_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send queue cancel request");
        assert_eq!(queue_cancel_response.status(), StatusCode::SEE_OTHER);
        let canceled_queue = app
            .apps
            .list_app_release_queue()
            .await
            .expect("list queue after generic cancel")
            .into_iter()
            .find(|item| item.id == queue_id)
            .expect("canceled queue");
        assert_eq!(canceled_queue.status, "canceled");

        let forbidden_publish = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/publish")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token=bad-token&release_id={}",
                        second_release.release_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send forbidden publish request");
        assert_eq!(forbidden_publish.status(), StatusCode::FORBIDDEN);

        let publish_response = app
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/artifacts/publish")
                    .header(header::COOKIE, &cookie_value)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={}&release_id={}",
                        login.session.csrf_token, second_release.release_id
                    )))
                    .expect("build request"),
            )
            .await
            .expect("send publish request");
        assert_eq!(publish_response.status(), StatusCode::SEE_OTHER);
        assert!(
            app.apps
                .list_app_release_queue()
                .await
                .expect("list queue after publish")
                .into_iter()
                .any(|item| {
                    item.release_id == second_release.release_id
                        && matches!(
                            item.status.as_str(),
                            "queued" | "running" | "success" | "failed"
                        )
                })
        );
    }

    #[tokio::test]
    async fn binary_releases_are_ordered_by_version_code_desc() {
        let app = test_web_app().await;
        let app_id = create_binary_test_app(&app.apps, "orders-api-prod").await;
        for (version, version_code) in [("v1.1.0", 1_001_000), ("v1.10.0", 1_010_000)] {
            app.apps
                .upload_binary_artifact(UploadBinaryArtifactInput {
                    app_id,
                    artifact_version: version.to_owned(),
                    version_code: Some(version_code),
                    published_at: "2026-06-09T10:00:00Z".to_owned(),
                    file_name: "orders-api-prod".to_owned(),
                    bytes: format!("orders binary {version}").into_bytes(),
                    entry_file: String::new(),
                    source: "ordering-test".to_owned(),
                })
                .await
                .expect("upload binary release");
        }

        let detail = app.apps.app_detail(app_id).await.expect("app detail");
        let releases = detail
            .binary_releases
            .iter()
            .map(|release| (release.version.as_str(), release.version_code))
            .collect::<Vec<_>>();

        assert_eq!(releases[0], ("v1.10.0", 1_010_000));
        assert_eq!(releases[1], ("v1.1.0", 1_001_000));
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
    fn live_task_refresh_waits_while_confirmation_dialog_is_open() {
        let template = include_str!("../../templates/task_detail.html");
        let script = include_str!("../../assets/app.js");

        assert!(template.contains("data-task-auto-refresh=\"3000\""));
        assert!(!template.contains("http-equiv=\"refresh\""));
        assert!(script.contains("dialog.modal-dialog[open]"));
        assert!(script.contains("initTaskAutoRefresh();"));
    }

    #[test]
    fn app_status_labels_split_enabled_and_runtime_state() {
        assert_eq!(app_enabled_status_label("ready"), "已启用");
        assert_eq!(app_enabled_status_label("draft"), "已启用");
        assert_eq!(app_enabled_status_label("disabled"), "已停用");
        assert_eq!(app_runtime_status_label("healthy"), "健康");
        assert_eq!(app_runtime_status_label("unknown"), "未部署");
        assert_eq!(
            normalize_app_runtime_status_filter(Some("draft")),
            "unknown"
        );
    }

    #[test]
    fn app_runtime_filter_uses_runtime_state_and_disabled_flag() {
        let app = crate::apps::AppListItem {
            id: 1,
            app_key: "orders-api".to_owned(),
            name: "订单服务".to_owned(),
            description: String::new(),
            environment: "test".to_owned(),
            app_type: "binary".to_owned(),
            deploy_mode: "binary".to_owned(),
            deploy_strategy: "rolling_stop_on_failure".to_owned(),
            release_source: "package_upload".to_owned(),
            compose_strategy: "recreate".to_owned(),
            auto_queue_release: 1,
            work_dir: "/opt/easy-deploy/apps/orders-api".to_owned(),
            status: "ready".to_owned(),
            runtime_status: "healthy".to_owned(),
            runtime_summary: "1 健康，0 异常，0 部署中，0 已停止，0 未知".to_owned(),
            target_names: Some("local".to_owned()),
            target_count: 1,
            created_at: "2026-06-01T00:00:00Z".to_owned(),
            updated_at: "2026-06-01T00:00:00Z".to_owned(),
        };

        assert!(app_matches_filters(&app, "binary", "test", "healthy", ""));
        assert!(!app_matches_filters(
            &app,
            "binary",
            "production",
            "healthy",
            ""
        ));
        assert!(!app_matches_filters(&app, "binary", "test", "unknown", ""));
        assert!(!app_matches_filters(&app, "compose", "test", "healthy", ""));

        let mut disabled_app = app.clone();
        disabled_app.status = "disabled".to_owned();
        disabled_app.runtime_status = "disabled".to_owned();
        assert!(app_matches_filters(
            &disabled_app,
            "binary",
            "test",
            "disabled",
            ""
        ));
        assert!(!app_matches_filters(
            &disabled_app,
            "binary",
            "test",
            "healthy",
            ""
        ));
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
            last_message: Some("SSH Docker daemon 不可用 Cannot connect".to_owned()),
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
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].title, "安装 Docker Engine");
        assert!(
            missing[0]
                .command
                .contains("ssh -p 22 -i /tmp/easy-deploy/id_ed25519 -o IdentitiesOnly=yes deploy@10.0.2.11 curl -fsSL https://get.docker.com")
        );

        node.docker_available = 1;
        node.last_docker_version = Some("Docker version 27.0.2".to_owned());
        node.last_message = Some("Docker Compose 不可用 plugin missing".to_owned());
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
        assert_eq!(ready[0].verify, "可以继续作为 Compose 部署目标使用");
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
