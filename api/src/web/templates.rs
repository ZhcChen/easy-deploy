use askama::Template;
use axum::response::{IntoResponse, Response};

use super::{HtmlTemplateError, html_response};

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub app_rows: &'a [AppRow],
    pub node_rows: &'a [NodeRow],
    pub task_rows: &'a [TaskRow],
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub bootstrap_required: bool,
    pub error_message: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "apps.html")]
pub struct AppsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub apps: &'a [AppPageRow<'a>],
    pub node_choices: &'a [AppNodeChoiceRow<'a>],
    pub selected_environment: &'a str,
    pub selected_status: &'a str,
    pub query: &'a str,
    pub filtered_count: usize,
    pub page: usize,
    pub total_pages: usize,
    pub page_start: usize,
    pub page_end: usize,
    pub prev_page_href: String,
    pub next_page_href: String,
    pub has_prev_page: bool,
    pub has_next_page: bool,
    pub default_app_work_dir: &'a str,
    pub default_app_work_dir_template: &'a str,
    pub can_manage: bool,
    pub can_toggle_status: bool,
}

#[derive(Template)]
#[template(path = "app_detail.html")]
pub struct AppDetailTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub id: i64,
    pub name: &'a str,
    pub app_key: &'a str,
    pub description: &'a str,
    pub app_type: &'a str,
    pub environment: &'a str,
    pub environment_label: &'a str,
    pub environment_tone: &'a str,
    pub deploy_strategy: &'a str,
    pub deploy_strategy_label: &'a str,
    pub release_source: &'a str,
    pub release_source_label: &'a str,
    pub auto_queue_release: bool,
    pub release_publish_mode: &'a str,
    pub work_dir: &'a str,
    pub runtime_root: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub targets: &'a str,
    pub target_count: i64,
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub compose_content: &'a str,
    pub env_content: &'a str,
    pub deploy_script_pre_deploy: &'a str,
    pub deploy_script_deploy: &'a str,
    pub deploy_script_post_deploy: &'a str,
    pub deploy_script_switch_traffic: &'a str,
    pub deploy_script_cleanup: &'a str,
    pub metadata_content: &'a str,
    pub health_check_kind: &'a str,
    pub health_check_label: &'a str,
    pub health_endpoint: &'a str,
    pub health_timeout_secs: u64,
    pub health_expected_status: u16,
    pub deployment_runs: &'a [AppDeploymentRunRow],
    pub deployment_environments: &'a [DeploymentEnvironmentRow],
    pub deployment_units: &'a [DeploymentUnitRow],
    pub application_releases: &'a [ApplicationReleaseRow],
    pub environment_runs: &'a [EnvironmentDeploymentRunRow],
    pub selected_environment_id: i64,
    pub config_snapshots: &'a [AppConfigSnapshotRow],
    pub deploy_diff: &'a AppDeployDiffView,
    pub runtime_states: &'a [AppRuntimeStateRow],
    pub target_choices: &'a [AppTargetChoiceRow],
    pub can_manage: bool,
    pub can_deploy: bool,
    pub can_logs: bool,
    pub compose_result: Option<ComposeResultView>,
    pub notice: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "deploy_confirm.html")]
pub struct DeployConfirmTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub app_id: i64,
    pub app_name: &'a str,
    pub app_key: &'a str,
    pub app_type: &'a str,
    pub work_dir: &'a str,
    pub action_label: &'a str,
    pub action_tone: &'a str,
    pub action_description: &'a str,
    pub post_action: String,
    pub targets: &'a str,
    pub target_count: i64,
    pub deploy_strategy: &'a str,
    pub plan_node_order: String,
    pub plan_failure_policy: &'a str,
    pub deploy_plan_steps: &'a [DeployPlanStepRow],
    pub deploy_plan_files: &'a [DeployPlanFileRow],
    pub preflight_summary: &'a str,
    pub preflight_summary_tone: &'a str,
    pub preflight_rows: &'a [DeployPreflightRow],
    pub preflight_can_submit: bool,
    pub preflight_submit_message: String,
    pub can_manage_nodes: bool,
    pub can_install_nodes: bool,
    pub target_nodes: &'a [DeployConfirmTargetNodeRow],
    pub health_check_label: &'a str,
    pub health_endpoint: &'a str,
    pub health_timeout_secs: u64,
    pub health_expected_status: u16,
    pub deploy_diff: &'a AppDeployDiffView,
}

pub struct DeployConfirmTargetNodeRow {
    pub name: String,
    pub node_key: String,
    pub node_type: &'static str,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub docker_status: String,
    pub preflight_hint: &'static str,
}

pub struct DeployPlanStepRow {
    pub label: &'static str,
    pub detail: String,
    pub tone: &'static str,
}

pub struct DeployPlanFileRow {
    pub label: &'static str,
    pub path: String,
    pub detail: &'static str,
}

pub struct DeployPreflightRow {
    pub node_id: i64,
    pub node_name: String,
    pub node_key: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub summary: String,
    pub checks: Vec<DeployPreflightCheckRow>,
    pub actions: Vec<DeployPreflightActionRow>,
}

pub struct DeployPreflightCheckRow {
    pub label: &'static str,
    pub result: &'static str,
    pub tone: &'static str,
    pub detail: String,
}

pub struct DeployPreflightActionRow {
    pub label: &'static str,
    pub action_kind: &'static str,
    pub component: &'static str,
}

#[derive(Template)]
#[template(path = "services.html")]
pub struct ServicesTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub services: &'a [ServicePageRow<'a>],
    pub service_count: usize,
    pub compose_count: usize,
    pub selected_status: &'a str,
    pub query: &'a str,
    pub can_logs: bool,
    pub can_retry: bool,
}

#[derive(Template)]
#[template(path = "service_logs.html")]
pub struct ServiceLogsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub app_id: i64,
    pub app_name: &'a str,
    pub service_name: &'a str,
    pub node_name: &'a str,
    pub node_key: &'a str,
    pub selected_node_id: i64,
    pub node_runtime_status: &'a str,
    pub node_runtime_status_tone: &'a str,
    pub node_runtime_summary: &'a str,
    pub node_active_version: &'a str,
    pub node_last_health_at: &'a str,
    pub node_last_message: &'a str,
    pub selected_task_href: &'a str,
    pub selected_task_action_label: &'a str,
    pub selected_task_id: i64,
    pub selected_task_return_to: &'a str,
    pub selected_can_retry_task: bool,
    pub has_selected_task: bool,
    pub node_links: &'a [ServiceNodeLinkRow],
    pub command: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub status_code: &'a str,
    pub output: &'a str,
    pub tail_lines: u16,
    pub tail_options: &'a [ServiceLogTailOptionRow],
}

pub struct ServiceLogTailOptionRow {
    pub label: String,
    pub href: String,
    pub active: bool,
}

#[derive(Template)]
#[template(path = "accounts.html")]
pub struct AccountsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub accounts: &'a [AccountRow<'a>],
    pub role_options: &'a [RoleOptionRow<'a>],
    pub status_filters: &'a [RbacFilterOptionRow],
    pub role_filters: &'a [RbacFilterOptionRow],
    pub query: &'a str,
    pub filtered_count: usize,
    pub can_manage: bool,
    pub notice: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "roles.html")]
pub struct RolesTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub roles: &'a [RoleRow<'a>],
    pub permission_groups: &'a [PermissionGroup<'a>],
    pub status_filters: &'a [RbacFilterOptionRow],
    pub module_filters: &'a [RbacFilterOptionRow],
    pub permission_dependencies_json: &'a str,
    pub query: &'a str,
    pub filtered_count: usize,
    pub can_manage: bool,
}

#[derive(Template)]
#[template(path = "permissions.html")]
pub struct PermissionsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub permission_groups: &'a [PermissionGroup<'a>],
    pub module_filters: &'a [RbacFilterOptionRow],
    pub type_filters: &'a [RbacFilterOptionRow],
    pub query: &'a str,
    pub filtered_count: usize,
}

#[derive(Template)]
#[template(path = "profile.html")]
pub struct ProfileTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub username: &'a str,
    pub display_name: &'a str,
    pub role_codes: &'a [String],
}

#[derive(Template)]
#[template(path = "sessions.html")]
pub struct SessionsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub sessions: &'a [SessionRow<'a>],
    pub status_filters: &'a [RbacFilterOptionRow],
    pub query: &'a str,
    pub filtered_count: usize,
    pub can_manage: bool,
    pub notice: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "api_tokens.html")]
pub struct ApiTokensTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub tokens: &'a [ApiTokenPageRow<'a>],
    pub created_token: Option<&'a str>,
    pub created_source: Option<&'a str>,
    pub created_prefix: Option<&'a str>,
    pub can_manage: bool,
    pub notice: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "nodes.html")]
pub struct NodesTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub nodes: &'a [NodePageRow<'a>],
    pub node_details: &'a [NodeDetailModalRow],
    pub selected_type: &'a str,
    pub selected_status: &'a str,
    pub query: &'a str,
    pub default_node_work_dir: &'a str,
    pub credential_options: &'a [NodeCredentialOptionRow],
    pub can_manage: bool,
}

pub struct NodeDetailModalRow {
    pub capability_guides: Vec<NodeCapabilityGuideRow>,
    pub checks: Vec<NodeCheckHistoryRow>,
    pub apps: Vec<NodeAppRuntimeRow>,
    pub tasks: Vec<NodeTaskRow>,
    pub can_install: bool,
}

#[derive(Template)]
#[template(path = "node_credentials.html")]
pub struct NodeCredentialsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub credentials: &'a [NodeCredentialPageRow<'a>],
    pub can_manage: bool,
}

#[derive(Template)]
#[template(path = "node_detail.html")]
pub struct NodeDetailTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub node: &'a NodePageRow<'a>,
    pub capability_guides: &'a [NodeCapabilityGuideRow],
    pub checks: &'a [NodeCheckHistoryRow],
    pub apps: &'a [NodeAppRuntimeRow],
    pub tasks: &'a [NodeTaskRow],
    pub can_manage: bool,
    pub can_install: bool,
}

#[derive(Template)]
#[template(path = "audit.html")]
pub struct AuditTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub logs: &'a [AuditLogRow<'a>],
    pub action_filters: &'a [AuditFilterOptionRow],
    pub target_filters: &'a [AuditFilterOptionRow],
    pub selected_action: &'a str,
    pub selected_target_type: &'a str,
    pub actor: &'a str,
    pub query: &'a str,
    pub filtered_count: usize,
}

#[derive(Template)]
#[template(path = "events.html")]
pub struct EventsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub logs: &'a [EventLogRow<'a>],
    pub event_type_filters: &'a [AuditFilterOptionRow],
    pub target_filters: &'a [AuditFilterOptionRow],
    pub selected_event_type: &'a str,
    pub selected_level: &'a str,
    pub selected_target_type: &'a str,
    pub query: &'a str,
    pub filtered_count: usize,
}

#[derive(Template)]
#[template(path = "tasks.html")]
pub struct TasksTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub tasks: &'a [TaskPageRow<'a>],
    pub status_filters: &'a [TaskFilterOptionRow],
    pub phase_filters: &'a [TaskFilterOptionRow],
    pub app_filters: &'a [TaskAppFilterRow<'a>],
    pub kind_filters: &'a [TaskFilterOptionRow],
    pub queue_summary: &'a str,
    pub selected_app_id: &'a str,
    pub query: &'a str,
    pub filtered_count: usize,
}

#[derive(Template)]
#[template(path = "templates.html")]
pub struct TemplatesTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub templates: &'a [TemplateCardRow<'a>],
}

#[derive(Template)]
#[template(path = "artifacts.html")]
pub struct ArtifactsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub artifacts: &'a [ArtifactPageRow],
    pub release_queue: &'a [ReleaseQueueRow],
    pub package_apps: &'a [ArtifactAppOptionRow],
    pub selected_status: &'a str,
    pub selected_kind: &'a str,
    pub selected_source: &'a str,
    pub query: &'a str,
    pub notice: &'a str,
    pub uploaded_binary_releases_to_keep: usize,
    pub can_upload: bool,
}

#[derive(Template)]
#[template(path = "task_detail.html")]
pub struct TaskDetailTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub task: TaskDetailView<'a>,
    pub execution_guide: TaskExecutionGuideView,
    pub return_action: TaskReturnActionView,
    pub phases: &'a [TaskPhaseStepRow],
    pub phase_groups: &'a [TaskPhaseGroupRow<'a>],
    pub node_results: &'a [TaskNodeResultRow<'a>],
    pub logs: &'a [TaskLogRow<'a>],
    pub can_retry: bool,
    pub can_cancel: bool,
    pub can_check_node: bool,
    pub can_install_nodes: bool,
    pub install_check_node_id: Option<i64>,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub summary_items: &'a [SummaryItem],
    pub runtime_rows: &'a [SettingsRow],
    pub storage_rows: &'a [SettingsRow],
    pub auth_rows: &'a [SettingsRow],
    pub deploy_rows: &'a [SettingsRow],
    pub can_update: bool,
    pub default_app_work_dir: &'a str,
    pub default_node_work_dir: &'a str,
    pub uploaded_binary_releases_to_keep: usize,
    pub artifact_storage_provider: &'a str,
    pub aliyun_oss_region: &'a str,
    pub aliyun_oss_endpoint: &'a str,
    pub aliyun_oss_bucket: &'a str,
    pub aliyun_oss_object_prefix: &'a str,
    pub aliyun_oss_access_key_id: &'a str,
    pub aliyun_oss_secret_status: &'a str,
    pub aliyun_oss_upload_url_ttl_seconds: i64,
    pub aliyun_oss_download_url_ttl_seconds: i64,
}

#[derive(Template)]
#[template(path = "application_deploy.html")]
pub struct ApplicationDeployTemplate<'a> {
    pub product_name: &'a str,
    pub css: &'a str,
    pub asset_version: &'a str,
    pub release_version: &'a str,
    pub current_user: &'a str,
    pub csrf_token: &'a str,
    pub nav_sections: &'a [NavSection<'a>],
    pub app_id: i64,
    pub app_name: &'a str,
    pub environment_id: i64,
    pub environment_name: &'a str,
    pub releases: &'a [ApplicationReleaseRow],
    pub selected_release_id: i64,
    pub mode: &'a str,
    pub mode_label: &'a str,
    pub plan_hash: &'a str,
    pub plan_rows: &'a [ApplicationDeployPlanRow],
    pub deploy_count: usize,
    pub skip_count: usize,
    pub stop_count: usize,
    pub has_active_run: bool,
    pub active_run_id: i64,
    pub executor_available: bool,
}

pub struct ApplicationDeployPlanRow {
    pub stage_no: i64,
    pub unit_key: String,
    pub version: String,
    pub action: &'static str,
    pub action_tone: &'static str,
    pub reason: String,
}

pub struct NavSection<'a> {
    pub label: &'a str,
    pub items: Vec<NavItem<'a>>,
}

pub struct NavItem<'a> {
    pub label: &'a str,
    pub href: &'a str,
    pub icon: &'a str,
    pub active: bool,
}

pub struct AccountRow<'a> {
    pub id: i64,
    pub is_current: bool,
    pub username: &'a str,
    pub display_name: &'a str,
    pub roles: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub security: String,
    pub security_tone: &'static str,
    pub active_session_count: i64,
    pub last_login_at: &'a str,
    pub toggle_label: &'a str,
    pub toggle_status: &'static str,
    pub role_choices: Vec<RoleChoiceRow<'a>>,
}

pub struct RoleRow<'a> {
    pub id: i64,
    pub is_system: bool,
    pub role_code: &'a str,
    pub role_name: &'a str,
    pub description: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub permission_count: i64,
    pub action_permission_count: usize,
    pub coverage_percent: usize,
    pub system_label: &'a str,
    pub toggle_label: &'a str,
    pub permission_groups: Vec<PermissionChoiceGroup<'a>>,
}

pub struct PermissionGroup<'a> {
    pub id: &'a str,
    pub module: &'a str,
    pub permissions: Vec<PermissionRow<'a>>,
}

#[derive(Clone, Copy)]
pub struct PermissionRow<'a> {
    pub id: i64,
    pub key: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub resource_type: &'a str,
    pub resource_tone: &'static str,
}

pub struct RoleOptionRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub code: &'a str,
}

pub struct RoleChoiceRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub checked: bool,
}

pub struct PermissionChoiceGroup<'a> {
    pub module: &'a str,
    pub permissions: Vec<PermissionChoiceRow<'a>>,
}

pub struct PermissionChoiceRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub key: &'a str,
    pub checked: bool,
}

pub struct SessionRow<'a> {
    pub id: i64,
    pub account: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub last_ip: &'a str,
    pub user_agent: &'a str,
    pub risk_label: &'static str,
    pub risk_tone: &'static str,
    pub created_at: &'a str,
    pub expires_at: &'a str,
    pub can_revoke: bool,
}

pub struct RbacFilterOptionRow {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

pub struct AuditLogRow<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub target: &'a str,
    pub message: &'a str,
    pub ip: &'a str,
    pub created_at: &'a str,
}

pub struct AuditFilterOptionRow {
    pub value: String,
    pub selected: bool,
}

pub struct EventLogRow<'a> {
    pub id: i64,
    pub event_type: &'a str,
    pub level: &'a str,
    pub level_tone: &'static str,
    pub target: String,
    pub title: &'a str,
    pub summary: &'a str,
    pub detail: &'a str,
    pub created_at: &'a str,
    pub has_detail: bool,
}

pub struct SummaryItem {
    pub label: &'static str,
    pub value: String,
    pub detail: String,
    pub tone: &'static str,
}

pub struct AppRow {
    pub name: String,
    pub stack: String,
    pub services: String,
    pub target: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub updated_at: String,
}

pub struct AppPageRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub app_key: &'a str,
    pub description: &'a str,
    pub environment: &'a str,
    pub environment_tone: &'a str,
    pub runtime_status: &'a str,
    pub runtime_status_tone: &'a str,
    pub enabled_status: &'a str,
    pub enabled_status_tone: &'a str,
    pub updated_at: &'a str,
    pub latest_version: String,
    pub deployment_status: &'static str,
    pub deployment_status_tone: &'static str,
    pub active_run_id: Option<i64>,
    pub active_task_id: Option<i64>,
    pub environment_id: Option<i64>,
    pub unit_count: i64,
    pub can_deploy: bool,
    pub toggle_status: &'static str,
    pub toggle_label: &'static str,
}

pub struct DeploymentEnvironmentRow {
    pub id: i64,
    pub name: String,
    pub key: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub runtime_status: &'static str,
    pub runtime_tone: &'static str,
    pub latest_version: String,
    pub target_count: i64,
    pub active_run_id: Option<i64>,
    pub active_task_id: Option<i64>,
    pub active_run_status: String,
    pub selected: bool,
}

pub struct DeploymentUnitRow {
    pub key: String,
    pub name: String,
    pub stage: String,
    pub lifecycle_status: &'static str,
    pub lifecycle_tone: &'static str,
    pub latest_version: String,
    pub runtime_status: String,
    pub runtime_tone: &'static str,
    pub work_dir: String,
}

pub struct ApplicationReleaseRow {
    pub id: i64,
    pub version: String,
    pub version_code: i64,
    pub unit_count: i64,
    pub created_at: String,
}

pub struct EnvironmentDeploymentRunRow {
    pub id: i64,
    pub task_id: Option<i64>,
    pub environment_name: String,
    pub version: String,
    pub mode: &'static str,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub result_summary: String,
    pub summary: String,
    pub created_at: String,
}

pub struct AppNodeChoiceRow<'a> {
    pub id: i64,
    pub label: &'a str,
    pub detail: &'a str,
}

pub struct AppTargetChoiceRow {
    pub id: i64,
    pub label: String,
    pub detail: String,
    pub checked: bool,
}

pub struct AppDeploymentRunRow {
    pub task_id: Option<i64>,
    pub title: String,
    pub action: &'static str,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub message: String,
    pub config_revision: String,
    pub artifact_version: String,
    pub started_at: String,
    pub finished_at: String,
}

pub struct AppConfigSnapshotRow {
    pub id: i64,
    pub revision: String,
    pub kind: &'static str,
    pub compose_summary: String,
    pub env_summary: String,
    pub artifact_version: String,
    pub config_hash: String,
    pub created_at: String,
    pub can_restore: bool,
}

pub struct AppDeployDiffView {
    pub status: &'static str,
    pub status_tone: &'static str,
    pub baseline: String,
    pub risk_title: String,
    pub risk_detail: String,
    pub changed_count: usize,
    pub empty_title: &'static str,
    pub empty_message: &'static str,
    pub rows: Vec<AppDeployDiffRow>,
}

pub struct AppDeployDiffRow {
    pub label: &'static str,
    pub current_summary: String,
    pub baseline_summary: String,
    pub current_preview: String,
    pub baseline_preview: String,
    pub has_detail: bool,
    pub status: &'static str,
    pub status_tone: &'static str,
}

pub struct AppRuntimeStateRow {
    pub node_name: String,
    pub node_key: String,
    pub node_detail_href: String,
    pub task_href: String,
    pub has_task_href: bool,
    pub log_links: Vec<ServiceNodeLinkRow>,
    pub has_log_links: bool,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub service_count: i64,
    pub active_version: String,
    pub message: String,
    pub last_deploy_at: String,
    pub updated_at: String,
}

pub struct ArtifactPageRow {
    pub id: i64,
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub version: String,
    pub version_code: i64,
    pub artifact_kind: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub queue_status: String,
    pub queue_status_tone: &'static str,
    pub publish_mode: &'static str,
    pub storage: String,
    pub storage_detail: String,
    pub sha256: String,
    pub size: String,
    pub entry_file: String,
    pub source: String,
    pub published_at: String,
    pub received_at: String,
    pub scheduled_publish_at: String,
    pub scheduled_publish_input: String,
    pub queue_id: Option<i64>,
    pub task_id: Option<i64>,
    pub can_publish_now: bool,
    pub app_deploying: bool,
    pub can_schedule: bool,
    pub can_cancel_schedule: bool,
    pub can_cancel_queue: bool,
}

pub struct ArtifactAppOptionRow {
    pub id: i64,
    pub label: String,
    pub detail: String,
}

pub struct ReleaseQueueRow {
    pub id: i64,
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub version: String,
    pub version_code: i64,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub queue_seq: i64,
    pub triggered_by: String,
    pub message: String,
    pub task_id: Option<i64>,
    pub scheduled_publish_at: String,
    pub created_at: String,
    pub started_at: String,
    pub finished_at: String,
    pub can_cancel: bool,
}

pub struct ServicePageRow<'a> {
    pub app_id: i64,
    pub app_name: &'a str,
    pub app_key: &'a str,
    pub service_name: &'a str,
    pub service_kind: &'a str,
    pub image: &'a str,
    pub ports: &'a str,
    pub replicas: &'a str,
    pub targets: &'a str,
    pub app_status: &'a str,
    pub runtime_status: &'a str,
    pub runtime_status_tone: &'a str,
    pub runtime_summary: &'a str,
    pub active_version: &'a str,
    pub health_check: &'a str,
    pub health_check_detail: String,
    pub health_status: &'static str,
    pub health_status_tone: &'static str,
    pub health_summary: &'static str,
    pub last_health_message: &'a str,
    pub last_health_at: &'a str,
    pub health_action_hint: &'static str,
    pub updated_at: &'a str,
    pub node_links: Vec<ServiceNodeLinkRow>,
}

#[derive(Clone, Debug)]
pub struct ServiceNodeLinkRow {
    pub name: String,
    pub node_key: String,
    pub href: String,
    pub node_href: String,
    pub task_href: String,
    pub task_id: i64,
    pub task_return_to: String,
    pub active: bool,
    pub runtime_status: &'static str,
    pub runtime_status_tone: &'static str,
    pub runtime_summary: String,
    pub task_status: &'static str,
    pub task_status_tone: &'static str,
    pub task_action_label: &'static str,
    pub active_version: String,
    pub last_health_at: String,
    pub message: String,
    pub has_task_href: bool,
    pub can_retry_task: bool,
}

#[derive(Clone, Debug)]
pub struct ComposeResultView {
    pub command: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub status_code: String,
    pub output: String,
}

pub struct NodeRow {
    pub name: String,
    pub address: String,
    pub region: String,
    pub load: String,
    pub status: &'static str,
    pub status_tone: &'static str,
}

pub struct NodePageRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub node_key: &'a str,
    pub node_type: &'a str,
    pub address: &'a str,
    pub ssh: String,
    pub ssh_port: i64,
    pub ssh_user: &'a str,
    pub credential_id: i64,
    pub credential_name: String,
    pub credential_fingerprint: String,
    pub work_dir: &'a str,
    pub region: &'a str,
    pub region_value: &'a str,
    pub labels: &'a str,
    pub labels_value: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub docker_status: &'a str,
    pub capability: String,
    pub os_info: String,
    pub disk_info: String,
    pub systemd_version: String,
    pub proxy_version: String,
    pub last_check_at: &'a str,
    pub last_message: &'a str,
    pub can_manage: bool,
    pub is_ssh: bool,
    pub can_check: bool,
    pub toggle_status: &'static str,
    pub toggle_label: &'static str,
}

pub struct NodeCredentialOptionRow {
    pub id: i64,
    pub label: String,
}

pub struct NodeCredentialPageRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub credential_key: &'a str,
    pub public_key: &'a str,
    pub fingerprint: &'a str,
    pub private_key_path: &'a str,
    pub passphrase_hint: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub created_by: &'a str,
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub bound_node_count: i64,
    pub toggle_status: &'static str,
    pub toggle_label: &'static str,
}

pub struct ApiTokenPageRow<'a> {
    pub id: i64,
    pub account: String,
    pub token_prefix: &'a str,
    pub source: &'a str,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub last_used_at: String,
    pub last_used_ip: &'a str,
    pub created_at: &'a str,
    pub revoked_at: &'a str,
    pub can_revoke: bool,
    pub can_delete: bool,
}

pub struct NodeCapabilityGuideRow {
    pub title: &'static str,
    pub tone: &'static str,
    pub reason: String,
    pub command: String,
    pub verify: &'static str,
    pub install_component: &'static str,
    pub can_install: bool,
}

pub struct NodeCheckHistoryRow {
    pub id: i64,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub message: String,
    pub docker_version: String,
    pub compose_version: String,
    pub os_info: String,
    pub disk_info: String,
    pub systemd_version: String,
    pub checked_at: String,
}

pub struct NodeAppRuntimeRow {
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub app_type: &'static str,
    pub app_status: &'static str,
    pub app_status_tone: &'static str,
    pub runtime_status: &'static str,
    pub runtime_status_tone: &'static str,
    pub active_version: String,
    pub service_count: i64,
    pub message: String,
    pub last_deploy_at: String,
    pub updated_at: String,
}

pub struct NodeTaskRow {
    pub id: i64,
    pub title: String,
    pub task_kind: &'static str,
    pub app_name: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub phase: &'static str,
    pub summary: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct TaskRow {
    pub title: String,
    pub target: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub time: String,
}

pub struct TaskPageRow<'a> {
    pub id: i64,
    pub title: &'a str,
    pub task_kind_label: &'static str,
    pub app_name: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub phase: &'a str,
    pub phase_tone: &'static str,
    pub queue_state: String,
    pub queue_tone: &'static str,
    pub command: &'a str,
    pub summary: &'a str,
    pub exit_code: String,
    pub created_by: &'a str,
    pub created_at: &'a str,
    pub updated_at: &'a str,
}

pub struct TaskFilterOptionRow {
    pub value: String,
    pub label: &'static str,
    pub count: i64,
    pub selected: bool,
}

pub struct TaskAppFilterRow<'a> {
    pub id: i64,
    pub name: &'a str,
    pub selected: bool,
}

pub struct TemplateCardRow<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub image: &'a str,
    pub default_port: u16,
    pub env_hint: &'a str,
}

pub struct SettingsRow {
    pub label: &'static str,
    pub value: String,
    pub detail: &'static str,
}

pub struct TaskDetailView<'a> {
    pub id: i64,
    pub title: &'a str,
    pub task_kind_label: &'static str,
    pub app_name: &'a str,
    pub node_name: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub phase: &'a str,
    pub phase_tone: &'static str,
    pub phase_detail: &'static str,
    pub queue_state: String,
    pub queue_tone: &'static str,
    pub command: &'a str,
    pub summary: &'a str,
    pub exit_code: String,
    pub created_by: &'a str,
    pub started_at: &'a str,
    pub finished_at: &'a str,
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub is_failed: bool,
    pub is_queued: bool,
    pub is_live: bool,
    pub is_retryable_task: bool,
}

pub struct TaskExecutionGuideView {
    pub title: String,
    pub tone: &'static str,
    pub detail: String,
    pub node_summary: String,
    pub log_hint: &'static str,
    pub next_step: String,
}

pub struct TaskReturnActionView {
    pub path: String,
    pub back_label: &'static str,
    pub check_label: &'static str,
    pub hint: &'static str,
    pub has_return: bool,
}

pub struct TaskPhaseStepRow {
    pub label: &'static str,
    pub state: &'static str,
    pub tone: &'static str,
}

pub struct TaskPhaseGroupRow<'a> {
    pub phase_no: i64,
    pub title: String,
    pub phase_key: String,
    pub status: &'static str,
    pub status_tone: &'static str,
    pub summary: String,
    pub started_at: String,
    pub finished_at: String,
    pub steps: Vec<TaskStepRow<'a>>,
    pub has_steps: bool,
    pub is_open: bool,
}

#[derive(Clone)]
pub struct TaskStepRow<'a> {
    pub step_no: i64,
    pub title: &'a str,
    pub node_name: &'a str,
    pub status: &'a str,
    pub status_tone: &'static str,
    pub command: &'a str,
    pub exit_code: String,
    pub started_at: &'a str,
    pub finished_at: &'a str,
    pub logs: Vec<TaskLogRow<'a>>,
    pub has_logs: bool,
    pub is_open: bool,
}

pub struct TaskNodeResultRow<'a> {
    pub node_id: i64,
    pub node_name: &'a str,
    pub node_key: &'a str,
    pub node_type: &'a str,
    pub status: &'a str,
    pub status_tone: &'a str,
    pub message: &'a str,
    pub command_count: i64,
    pub finished_at: &'a str,
    pub action_kind: &'static str,
    pub action_label: &'static str,
    pub action_component: &'static str,
    pub action_hint: &'static str,
    pub has_action: bool,
}

#[derive(Clone)]
pub struct TaskLogRow<'a> {
    pub id: i64,
    pub stream: &'a str,
    pub stream_tone: &'a str,
    pub content: &'a str,
    pub created_at: &'a str,
}

pub fn render_html<T>(template: T) -> Response
where
    T: Template,
{
    match template.render() {
        Ok(html) => html_response(html),
        Err(err) => HtmlTemplateError::from(err).into_response(),
    }
}
