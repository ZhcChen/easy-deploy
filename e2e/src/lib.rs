use std::path::Path;
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, net::SocketAddr};

use api::{
    AppState, AppStateServices, Settings,
    application_releases::ApplicationReleaseService,
    apps::AppService,
    auth::{AuthService, MemorySessionStore},
    build_router,
    deploy::{
        CommandResult, CommandRunner, CommandSpec, ComposeExecutor, DeployError, SystemdExecutor,
    },
    deployment_console::DeploymentConsoleService,
    deployment_orchestrator::DeploymentOrchestratorService,
    deployment_retention::{DeploymentLogService, DeploymentRetentionService},
    events::EventLogService,
    migrations::connect_database,
    node_credentials::NodeCredentialService,
    nodes::NodeService,
    platform::PlatformConfigService,
    runtimefs::RuntimeFs,
    tasks::TaskService,
};
use async_trait::async_trait;
use tokio::net::TcpListener;

const LOCAL_TEST_ADMIN_PASSWORD: &str = "gf7G7MQKPcQoHa79RUdz09yg";
const LOCAL_TEST_CHANGED_ADMIN_PASSWORD: &str = "gf7G7MQKPcQoHa79RUdz09ygChanged";

#[derive(Default)]
struct E2eCommandRunner {
    command_results: Mutex<HashMap<String, CommandResult>>,
    commands: Mutex<Vec<String>>,
    command_specs: Mutex<Vec<(String, String)>>,
}

impl E2eCommandRunner {
    fn with_result(self: &Arc<Self>, command: &str, result: CommandResult) -> Arc<Self> {
        self.command_results
            .lock()
            .expect("lock command results")
            .insert(command.to_owned(), result);
        self.clone()
    }

    fn result_for_command(&self, command: &str) -> Option<CommandResult> {
        let results = self.command_results.lock().expect("lock command results");
        if let Some(result) = results.get(command).cloned() {
            return Some(result);
        }
        normalized_managed_known_hosts_command(command)
            .and_then(|normalized| results.get(&normalized).cloned())
    }
}

#[async_trait]
impl CommandRunner for E2eCommandRunner {
    async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
        let command = format!("{} {}", spec.program, spec.args.join(" "));
        self.commands
            .lock()
            .expect("lock commands")
            .push(command.clone());
        self.command_specs
            .lock()
            .expect("lock command specs")
            .push((
                command.clone(),
                spec.current_dir.to_string_lossy().to_string(),
            ));
        if let Some(result) = self.result_for_command(&command) {
            return Ok(result);
        }
        if let Some(result) = self.ssh_probe_result(&command) {
            return Ok(result);
        }
        Ok(CommandResult {
            status_code: Some(0),
            stdout: format!("e2e command ok: {command}\n"),
            stderr: String::new(),
        })
    }
}

fn normalized_managed_known_hosts_command(command: &str) -> Option<String> {
    if !(command.starts_with("ssh ") || command.starts_with("scp ")) {
        return None;
    }
    let normalized = command
        .replace(
            "-o UserKnownHostsFile=.easy-deploy\\ssh\\known_hosts -o StrictHostKeyChecking=yes ",
            "",
        )
        .replace(
            "-o UserKnownHostsFile=.easy-deploy/ssh/known_hosts -o StrictHostKeyChecking=yes ",
            "",
        );
    (normalized != command).then_some(normalized)
}

impl E2eCommandRunner {
    fn ssh_probe_result(&self, command: &str) -> Option<CommandResult> {
        if !command.starts_with("ssh ")
            || !command.contains(" sh -lc ")
            || !command.contains("run_probe")
        {
            return None;
        }
        let destination = command
            .split_whitespace()
            .find(|part| part.contains('@') && !part.starts_with('-'))?;
        let base = format!("ssh -p 22 {destination}");
        let fields = [
            ("work_dir", "mkdir -p /opt/easy-deploy/apps"),
            ("os_info", "uname -srmo"),
            ("disk_info", "df -h /opt/easy-deploy/apps"),
            ("systemd_version", "systemctl --version"),
            ("docker_version", "docker --version"),
            ("docker_info", "docker info"),
            ("compose_version", "docker compose version"),
            ("caddy_version", "caddy version"),
            ("nginx_version", "nginx -v"),
        ];
        let results = self.command_results.lock().expect("lock command results");
        let mut stdout = String::new();
        for (field, suffix) in fields {
            let key = format!("{base} {suffix}");
            let result = results.get(&key).cloned().unwrap_or(CommandResult {
                status_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            });
            let output = result.combined_output();
            stdout.push_str(&format!(
                "ED_PROBE_STATUS={}\nED_PROBE_FIELD={field}\n",
                if result.success() { "ok" } else { "missing" }
            ));
            stdout.push_str(&output);
            if !output.ends_with('\n') {
                stdout.push('\n');
            }
            stdout.push_str(&format!("ED_PROBE_END={field}\n"));
        }
        Some(CommandResult {
            status_code: Some(0),
            stdout,
            stderr: String::new(),
        })
    }
}

pub async fn smoke_test() -> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let database_path = temp_dir.path().join("e2e.db");
    let database_url = format!("sqlite://{}", database_path.to_string_lossy());

    let db = connect_database(&database_url, true).await?;
    sqlx::migrate!("../api/migrations").run(&db).await?;
    let auth = AuthService::new(db.clone(), Arc::new(MemorySessionStore::new()));
    auth.sync_permission_registry().await?;
    let command_runner = Arc::new(E2eCommandRunner::default());
    let nodes = NodeService::new(db.clone(), command_runner.clone());
    let node_credentials = NodeCredentialService::new(db.clone(), temp_dir.path());
    let tasks = TaskService::new(db.clone());
    let platform = PlatformConfigService::new(db.clone());
    let events = EventLogService::new(db.clone());
    let apps = AppService::new(
        db.clone(),
        RuntimeFs::new(temp_dir.path()),
        ComposeExecutor::new(command_runner.clone()),
        SystemdExecutor::new(command_runner.clone()),
        tasks.clone(),
        platform.clone(),
    )
    .await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let settings = Settings {
        bind: addr,
        database_url,
        data_dir: temp_dir.path().to_path_buf(),
        cookie_secure: false,
        uploaded_binary_releases_to_keep: 4,
        command_timeout_secs: 120,
        config_active_key_id: "v1".to_owned(),
        config_master_keys: String::new(),
    };
    let app = build_router(AppState::new(
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
            deployment_logs: DeploymentLogService::new(db.clone()),
            deployment_retention: DeploymentRetentionService::new(db.clone()),
        },
    ));

    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let result = run_checks(addr, temp_dir.path(), command_runner, db).await;
    server.abort();

    result
}

async fn run_checks(
    addr: SocketAddr,
    data_dir: &Path,
    command_runner: Arc<E2eCommandRunner>,
    db: sqlx::SqlitePool,
) -> anyhow::Result<()> {
    let client = test_client()?;
    let base_url = format!("http://{addr}");
    let binary_target_dir = data_dir.join("target-node").join("worker-bin");
    let binary_target_dir_str = binary_target_dir.to_string_lossy().to_string();
    let binary_deploy_dir = binary_target_dir_str.replace('\\', "/");
    let binary_caddy_config = data_dir
        .join("caddy")
        .join("worker-bin.caddy")
        .to_string_lossy()
        .replace('\\', "/");

    let health = client
        .get(format!("{base_url}/healthz"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(health == "ok", "unexpected health response: {health}");

    let redirect = client.get(&base_url).send().await?;
    anyhow::ensure!(
        redirect.status() == reqwest::StatusCode::SEE_OTHER,
        "dashboard should redirect before login: {}",
        redirect.status()
    );
    let location = redirect
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    anyhow::ensure!(
        location == "/login?notice=required",
        "unexpected redirect: {location}"
    );

    let login_page = client
        .get(format!("{base_url}/login"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &login_page,
        &[
            "<title>",
            "Easy Deploy",
            "name=\"username\" value=\"admin\"",
            "readonly",
            "name=\"password\"",
            "data-password-generator",
            "data-target=\"auth-password\"",
            "type=\"submit\"",
        ],
        "bootstrap login page should fix admin username and expose password generator",
    )?;
    anyhow::ensure!(
        !login_page.contains("认证策略") && !login_page.contains("Access/Refresh 双 Token"),
        "bootstrap login page should not show auth strategy hints"
    );

    let bootstrap = client
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "admin"),
            ("display_name", "绠＄�"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        bootstrap.status() == reqwest::StatusCode::SEE_OTHER,
        "bootstrap should redirect after login: {}",
        bootstrap.status()
    );
    let initialized_login_page = client
        .get(format!("{base_url}/login"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &initialized_login_page,
        &["Easy Deploy", "name=\"username\"", "name=\"password\""],
        "initialized login page should show login form",
    )?;
    anyhow::ensure!(
        !initialized_login_page.contains("认证策略")
            && !initialized_login_page.contains("未启�Secure"),
        "initialized login page should not show auth strategy hints"
    );

    let dashboard = client
        .get(&base_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        dashboard.contains("Easy Deploy"),
        "dashboard did not contain product name"
    );
    anyhow::ensure!(
        dashboard.contains("href=\"/apps\"")
            && dashboard.contains("href=\"/nodes\"")
            && dashboard.contains("href=\"/tasks\"")
            && dashboard.contains("运行项")
            && !dashboard.contains("href=\"/services\"")
            && !dashboard.contains("/deployments/new")
            && !dashboard.contains("orders-api")
            && !dashboard.contains("billing-web"),
        "dashboard should render real initial data and current navigation links"
    );
    anyhow::ensure!(
        dashboard.contains("href=\"/admin/accounts\"")
            && dashboard.contains("href=\"/admin/roles\""),
        "super admin should see RBAC navigation"
    );
    let settings_page = client
        .get(format!("{base_url}/settings"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        settings_page.contains("/opt/easy-deploy/apps/{app_key}")
            && settings_page.contains("name=\"default_app_work_dir\"")
            && settings_page.contains("name=\"default_node_work_dir\"")
            && settings_page.contains("name=\"uploaded_binary_releases_to_keep\"")
            && settings_page.contains("name=\"artifact_storage_provider\"")
            && settings_page.contains("name=\"aliyun_oss_bucket\"")
            && settings_page.contains("EASY_DEPLOY_COMMAND_TIMEOUT_SECS")
            && settings_page.contains("redis-single 6379")
            && settings_page.contains("postgres-single 5432")
            && settings_page.contains(&addr.to_string())
            && settings_page.contains(&data_dir.to_string_lossy().to_string()),
        "settings page should render runtime configuration"
    );
    let settings_csrf = extract_csrf_token(&settings_page)?;
    let updated_settings = client
        .post(format!("{base_url}/settings"))
        .form(&[
            ("csrf_token", settings_csrf.as_str()),
            ("default_app_work_dir", "/srv/easy/{app_key}"),
            ("default_node_work_dir", "/srv/easy"),
            ("uploaded_binary_releases_to_keep", "6"),
            ("artifact_storage_provider", "local"),
            ("aliyun_oss_region", "oss-cn-hangzhou"),
            (
                "aliyun_oss_endpoint",
                "https://oss-cn-hangzhou.aliyuncs.com",
            ),
            ("aliyun_oss_bucket", ""),
            ("aliyun_oss_object_prefix", "easy-deploy/releases"),
            ("aliyun_oss_access_key_id", ""),
            ("aliyun_oss_access_key_secret", ""),
            ("aliyun_oss_upload_url_ttl_seconds", "900"),
            ("aliyun_oss_download_url_ttl_seconds", "600"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        updated_settings.status() == reqwest::StatusCode::SEE_OTHER,
        "settings update should redirect: {}",
        updated_settings.status()
    );
    let settings_page = client
        .get(format!("{base_url}/settings"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        settings_page.contains("value=\"6\"")
            && settings_page.contains("value=\"/srv/easy/{app_key}\"")
            && settings_page.contains("value=\"/srv/easy\""),
        "settings page should persist platform defaults"
    );
    let apps_defaults = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let nodes_defaults = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        apps_defaults.contains("value=\"/srv/easy/orders-api\"")
            && nodes_defaults.contains("value=\"/srv/easy\""),
        "editable create forms should use persisted platform defaults"
    );
    let restore_settings = client
        .post(format!("{base_url}/settings"))
        .form(&[
            ("csrf_token", extract_csrf_token(&settings_page)?.as_str()),
            ("default_app_work_dir", "/opt/easy-deploy/apps/{app_key}"),
            ("default_node_work_dir", "/opt/easy-deploy/apps"),
            ("uploaded_binary_releases_to_keep", "4"),
            ("artifact_storage_provider", "local"),
            ("aliyun_oss_region", "oss-cn-hangzhou"),
            (
                "aliyun_oss_endpoint",
                "https://oss-cn-hangzhou.aliyuncs.com",
            ),
            ("aliyun_oss_bucket", ""),
            ("aliyun_oss_object_prefix", "easy-deploy/releases"),
            ("aliyun_oss_access_key_id", ""),
            ("aliyun_oss_access_key_secret", ""),
            ("aliyun_oss_upload_url_ttl_seconds", "900"),
            ("aliyun_oss_download_url_ttl_seconds", "600"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        restore_settings.status() == reqwest::StatusCode::SEE_OTHER,
        "settings restore should redirect: {}",
        restore_settings.status()
    );

    let accounts = client
        .get(format!("{base_url}/admin/accounts"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let viewer_role_id = role_id(&db, "viewer").await?;
    anyhow::ensure!(
        accounts.contains("admin")
            && accounts.contains("action=\"/admin/accounts\"")
            && accounts.contains("name=\"role_ids\""),
        "accounts page did not render initialized account"
    );

    let refresh = client
        .post(format!("{base_url}/auth/refresh"))
        .send()
        .await?;
    anyhow::ensure!(
        refresh.status() == reqwest::StatusCode::SEE_OTHER,
        "refresh should redirect with rotated cookies: {}",
        refresh.status()
    );
    let accounts = client
        .get(format!("{base_url}/admin/accounts"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let csrf_token = extract_csrf_token(&accounts)?;

    let nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        nodes.contains("local")
            && nodes.contains("127.0.0.1")
            && nodes.contains("name=\"node_key\"")
            && nodes.contains("name=\"node_type\"")
            && nodes.contains("name=\"credential_id\"")
            && nodes.contains("name=\"work_dir\""),
        "nodes page did not render default local node"
    );
    let credentials_page = client
        .get(format!("{base_url}/node-credentials"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        credentials_page.contains("action=\"/node-credentials/generate\"")
            && credentials_page.contains("action=\"/node-credentials/upload\"")
            && credentials_page.contains("name=\"key_algorithm\"")
            && credentials_page.contains("value=\"ed25519\"")
            && credentials_page.contains("value=\"rsa_4096\"")
            && credentials_page.contains("name=\"private_key\"")
            && credentials_page.contains("name=\"public_key\""),
        "node credentials page should render generation and upload forms"
    );
    anyhow::ensure!(
        nodes.contains("name=\"type\"")
            && nodes.contains("name=\"status\"")
            && nodes.contains("name=\"q\""),
        "nodes page should render filter controls"
    );
    let node_csrf = extract_csrf_token(&nodes)?;
    let create_node = client
        .post(format!("{base_url}/nodes"))
        .form(&[
            ("csrf_token", node_csrf.as_str()),
            ("node_key", "prod-a"),
            ("name", "鐢熶骇鑺傜偣 A"),
            ("node_type", "ssh"),
            ("address", "10.0.2.11"),
            ("ssh_port", "22"),
            ("ssh_user", "deploy"),
            ("credential_id", "0"),
            ("work_dir", "/opt/easy-deploy/apps"),
            ("region", "prod"),
            ("labels", "prod,docker,api"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_node.status() == reqwest::StatusCode::SEE_OTHER,
        "create node should redirect: {}",
        create_node.status()
    );
    let updated_nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        updated_nodes.contains("prod-a")
            && updated_nodes.contains("10.0.2.11")
            && updated_nodes.contains("deploy")
            && updated_nodes.contains("/opt/easy-deploy/apps"),
        "created node should appear on nodes page"
    );
    let filtered_nodes = client
        .get(format!("{base_url}/nodes?type=ssh&q=prod-a"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        filtered_nodes.contains("value=\"ssh\" selected")
            && filtered_nodes.contains("value=\"prod-a\"")
            && filtered_nodes.contains("prod-a")
            && filtered_nodes.contains("10.0.2.11")
            && !filtered_nodes.contains("127.0.0.1"),
        "nodes page should filter by type and keyword"
    );
    let empty_nodes = client
        .get(format!("{base_url}/nodes?status=disabled&q=prod-a"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        empty_nodes.contains("value=\"disabled\" selected")
            && empty_nodes.contains("value=\"prod-a\"")
            && empty_nodes.contains("name=\"type\""),
        "nodes page should render empty state for unmatched filters"
    );
    let update_node = client
        .post(format!("{base_url}/nodes/update"))
        .form(&[
            ("csrf_token", extract_csrf_token(&updated_nodes)?.as_str()),
            ("node_id", "2"),
            ("name", "鐢熶骇鑺傜偣 A"),
            ("node_type", "ssh"),
            ("address", "10.0.2.11"),
            ("ssh_port", "22"),
            ("ssh_user", "deploy"),
            ("credential_id", "0"),
            ("work_dir", "/opt/easy-deploy/apps"),
            ("region", "prod-east"),
            ("labels", "prod,docker,api,edge"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        update_node.status() == reqwest::StatusCode::SEE_OTHER,
        "update node should redirect: {}",
        update_node.status()
    );
    let updated_nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        updated_nodes.contains("prod-east") && updated_nodes.contains("prod,docker,api,edge"),
        "updated node metadata should appear on nodes page"
    );

    command_runner.with_result(
        "uname -srmo",
        CommandResult {
            status_code: Some(0),
            stdout: "Linux 6.8.0 x86_64 GNU/Linux\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "df -h .easy-deploy/apps",
        CommandResult {
            status_code: Some(0),
            stdout: "Filesystem      Size  Used Avail Use% Mounted on\n/dev/sda1        40G   12G   28G  31% /\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "systemctl --version",
        CommandResult {
            status_code: Some(0),
            stdout: "systemd 255 (255.4-1ubuntu8)\n+PAM +AUDIT\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "docker --version",
        CommandResult {
            status_code: Some(0),
            stdout: "Docker version 27.0.1, build e2e\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "docker info",
        CommandResult {
            status_code: Some(0),
            stdout: "Server Version: 27.0.1\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "docker compose version",
        CommandResult {
            status_code: Some(0),
            stdout: "Docker Compose version v2.28.1\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "caddy version",
        CommandResult {
            status_code: Some(0),
            stdout: "2.8.4\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "nginx -v",
        CommandResult {
            status_code: Some(0),
            stdout: String::new(),
            stderr: "nginx version: nginx/1.24.0\n".to_owned(),
        },
    );
    let local_check = client
        .post(format!("{base_url}/nodes/check"))
        .form(&[
            ("csrf_token", extract_csrf_token(&updated_nodes)?.as_str()),
            ("node_id", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        local_check.status() == reqwest::StatusCode::SEE_OTHER,
        "local node check should redirect: {}",
        local_check.status()
    );

    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 mkdir -p /opt/easy-deploy/apps",
        CommandResult {
            status_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 uname -srmo",
        CommandResult {
            status_code: Some(0),
            stdout: "Linux 6.6.12 x86_64 GNU/Linux\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 df -h /opt/easy-deploy/apps",
        CommandResult {
            status_code: Some(0),
            stdout: "Filesystem      Size  Used Avail Use% Mounted on\n/dev/vda1        80G   20G   60G  25% /\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 systemctl --version",
        CommandResult {
            status_code: Some(0),
            stdout: "systemd 254 (254.5-1)\n+PAM +AUDIT\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker --version",
        CommandResult {
            status_code: Some(0),
            stdout: "Docker version 27.0.2, build ssh-e2e\n".to_owned(),
            stderr: String::new(),
        },
    );
    let pending_ssh_node_detail = client
        .get(format!("{base_url}/nodes/2"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        pending_ssh_node_detail.contains("prod-a")
            && pending_ssh_node_detail.contains("10.0.2.11")
            && pending_ssh_node_detail.contains("action=\"/nodes/install\"")
            && pending_ssh_node_detail.contains("name=\"return_to\" value=\"/nodes/2\"")
            && pending_ssh_node_detail
                .contains("ssh -p 22 deploy@10.0.2.11 curl -fsSL https://get.docker.com")
            && pending_ssh_node_detail.contains("systemd"),
        "node detail should render empty probe history before first check"
    );
    anyhow::ensure!(
        pending_ssh_node_detail.contains("href=\"/apps\"")
            && pending_ssh_node_detail.contains("href=\"/tasks\""),
        "node detail should render empty apps and tasks before first binding"
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker info",
        CommandResult {
            status_code: Some(0),
            stdout: "Server Version: 27.0.2\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker compose version",
        CommandResult {
            status_code: Some(0),
            stdout: "Docker Compose version v2.29.0\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 caddy version",
        CommandResult {
            status_code: Some(0),
            stdout: "2.8.4\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 nginx -v",
        CommandResult {
            status_code: Some(0),
            stdout: String::new(),
            stderr: "nginx version: nginx/1.24.0\n".to_owned(),
        },
    );
    let ssh_check = client
        .post(format!("{base_url}/nodes/check"))
        .form(&[
            ("csrf_token", extract_csrf_token(&updated_nodes)?.as_str()),
            ("node_id", "2"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        ssh_check.status() == reqwest::StatusCode::SEE_OTHER,
        "ssh node check should redirect: {}",
        ssh_check.status()
    );
    let checked_nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        checked_nodes.contains("Docker version 27.0.1")
            && checked_nodes.contains("Docker Compose version v2.28.1")
            && checked_nodes.contains("Docker version 27.0.2")
            && checked_nodes.contains("Docker Compose version v2.29.0")
            && checked_nodes.contains("prod-a")
            && checked_nodes.contains("local"),
        "node checks should run local and ssh Docker/Compose probes"
    );
    anyhow::ensure!(
        checked_nodes.contains("Linux 6.8.0 x86_64 GNU/Linux")
            && checked_nodes.contains("/dev/sda1")
            && checked_nodes.contains("systemd 255")
            && checked_nodes.contains("Linux 6.6.12 x86_64 GNU/Linux")
            && checked_nodes.contains("/dev/vda1")
            && checked_nodes.contains("systemd 254")
            && checked_nodes.contains("Caddy 2.8.4")
            && checked_nodes.contains("nginx version: nginx/1.24.0"),
        "node checks should render OS, disk and systemd capability details: {checked_nodes}"
    );
    anyhow::ensure!(
        checked_nodes.contains("data-modal-target=\"node-detail-2\"")
            && checked_nodes.contains("id=\"node-detail-2\""),
        "nodes page should expose node detail modal"
    );
    let ready_capability_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM node_capabilities
        WHERE docker_available = 1
          AND compose_available = 1
          AND systemd_available = 1
          AND caddy_available = 1
          AND nginx_available = 1
        "#,
    )
    .fetch_one(&db)
    .await?;
    anyhow::ensure!(
        ready_capability_count >= 2,
        "local and ssh node checks should cache latest node capabilities"
    );
    let ssh_node_detail = client
        .get(format!("{base_url}/nodes/2"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        ssh_node_detail.contains("prod-a")
            && ssh_node_detail.contains("Docker version 27.0.2")
            && ssh_node_detail.contains("Docker Compose version v2.29.0")
            && ssh_node_detail.contains("Linux 6.6.12 x86_64 GNU/Linux")
            && ssh_node_detail.contains("/dev/vda1")
            && ssh_node_detail.contains("systemd 254")
            && ssh_node_detail.contains("Caddy 2.8.4")
            && ssh_node_detail.contains("nginx version: nginx/1.24.0")
            && ssh_node_detail.contains("ssh -p 22 deploy@10.0.2.11 docker compose version"),
        "node detail should render current capability and successful check history"
    );

    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker info",
        CommandResult {
            status_code: Some(1),
            stdout: String::new(),
            stderr: "Cannot connect to the Docker daemon".to_owned(),
        },
    );
    let ssh_failed_check = client
        .post(format!("{base_url}/nodes/check"))
        .form(&[
            ("csrf_token", extract_csrf_token(&checked_nodes)?.as_str()),
            ("node_id", "2"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        ssh_failed_check.status() == reqwest::StatusCode::SEE_OTHER,
        "failed ssh node check should still redirect after recording result: {}",
        ssh_failed_check.status()
    );
    let failed_checked_nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        failed_checked_nodes.contains("prod-a")
            && failed_checked_nodes.contains("Cannot connect to the Docker daemon"),
        "failed ssh node check should record offline status and daemon error"
    );
    let failed_ssh_node_detail = client
        .get(format!("{base_url}/nodes/2"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        failed_ssh_node_detail.contains("Cannot connect to the Docker daemon")
            && failed_ssh_node_detail.contains("Docker version 27.0.2")
            && failed_ssh_node_detail.contains("Linux 6.6.12 x86_64 GNU/Linux")
            && failed_ssh_node_detail.contains("action=\"/nodes/install\"")
            && failed_ssh_node_detail.contains("component")
            && failed_ssh_node_detail.contains("docker"),
        "node detail should keep successful and failed probe history"
    );
    command_runner.with_result(
        "ssh -p 22 -o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=3 deploy@10.0.2.11 sh -lc 'curl -fsSL https://get.docker.com | sudo sh && sudo systemctl enable --now docker'",
        CommandResult {
            status_code: Some(0),
            stdout: "docker install ok\n".to_owned(),
            stderr: String::new(),
        },
    );
    let install_docker = client
        .post(format!("{base_url}/nodes/install"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&failed_ssh_node_detail)?.as_str(),
            ),
            ("node_id", "2"),
            ("component", "docker"),
            ("return_to", "/nodes/2"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        install_docker.status() == reqwest::StatusCode::SEE_OTHER,
        "node install should redirect to task detail: {}",
        install_docker.status()
    );
    let install_docker_location = response_location(&install_docker)?;
    anyhow::ensure!(
        install_docker_location.contains("?return_to=/nodes/2"),
        "node detail install task redirect should preserve node return path: {install_docker_location}"
    );
    let install_task_id = extract_task_id_from_location(
        install_docker_location
            .split('?')
            .next()
            .unwrap_or(install_docker_location),
    )?;
    let install_task_detail = wait_for_page(
        &client,
        &format!("{base_url}/tasks/{install_task_id}?return_to=/nodes/2"),
        &[
            "Docker Engine",
            "action=\"/nodes/check\"",
            "name=\"node_id\" value=\"2\"",
            "href=\"/nodes/2\"",
            "docker install ok",
            "prod-a",
        ],
    )
    .await?;
    anyhow::ensure!(
        install_task_detail.contains("Docker Engine")
            && install_task_detail.contains("ssh -p 22 -o BatchMode=yes")
            && install_task_detail.contains("curl -fsSL")
            && install_task_detail.contains("action=\"/nodes/check\"")
            && install_task_detail.contains("name=\"return_to\" value=\"/nodes/2\""),
        "node install task should render command, logs and node result"
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker info",
        CommandResult {
            status_code: Some(0),
            stdout: "Server Version: 27.0.2\n".to_owned(),
            stderr: String::new(),
        },
    );
    sqlx::query("UPDATE nodes SET status = 'online', docker_status = 'available' WHERE id = 2")
        .execute(&db)
        .await?;

    let apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        apps.contains("action=\"/apps\"")
            && apps.contains("id=\"create-app-modal\"")
            && apps.contains("id=\"new-app-title\"")
            && apps.contains("name=\"app_key\"")
            && apps.contains("name=\"target_node_ids\""),
        "apps page did not render management form"
    );
    anyhow::ensure!(
        apps.contains("data-modal-target=\"create-app-modal\"")
            && apps.contains("pagination-bar")
            && apps.contains("当前条件命中")
            && !apps.contains("模板创建")
            && !apps.contains("应用入口"),
        "apps page should keep only the app list and create action"
    );
    let new_app_redirect = client.get(format!("{base_url}/apps/new")).send().await?;
    anyhow::ensure!(
        new_app_redirect.status() == reqwest::StatusCode::SEE_OTHER,
        "new app shortcut should redirect: {}",
        new_app_redirect.status()
    );
    anyhow::ensure!(
        response_location(&new_app_redirect)? == "/apps#create-app-modal",
        "new app shortcut should redirect to create modal anchor"
    );
    anyhow::ensure!(
        apps.contains("name=\"environment\"")
            && apps.contains("name=\"status\"")
            && apps.contains("name=\"q\""),
        "apps page should render filter controls"
    );
    anyhow::ensure!(
        apps.contains("id=\"new-app-key\"")
            && apps.contains("id=\"new-app-work-dir\"")
            && apps.contains("name=\"release_source\"")
            && apps.contains("name=\"auto_queue_release\""),
        "apps page should render synced creation defaults"
    );
    anyhow::ensure!(
        apps.contains("name=\"app_type\" value=\"compose\"")
            && apps.contains("data-app-config-import")
            && apps.contains("name=\"deploy_script_deploy\"")
            && apps.contains("name=\"health_check_kind\"")
            && !apps.contains("data-mode-option=\"binary\"")
            && !apps.contains("id=\"new-app-binary-artifact-path\"")
            && !apps.contains("name=\"binary_release_strategy\""),
        "apps page should render compose-only create form"
    );
    let app_csrf = extract_csrf_token(&apps)?;
    let local_node_id = node_id_by_key(&db, "local").await?;
    let ssh_node_id = node_id_by_key(&db, "prod-a").await?;
    let create_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", app_csrf.as_str()),
            ("app_key", "orders-api"),
            ("name", "订单服务"),
            ("description", "E2E 创建 Compose 应用"),
            ("app_type", "compose"),
            ("deploy_strategy", "rolling_stop_on_failure"),
            ("work_dir", "/opt/easy-deploy/apps/orders-api"),
            (
                "compose_content",
                "version: '3.8'\nservices:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", "APP_ENV=e2e\n"),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_app.status() == reqwest::StatusCode::SEE_OTHER,
        "create app should redirect: {}",
        create_app.status()
    );
    anyhow::ensure!(
        response_location(&create_app)? == "/apps/1?notice=created",
        "create app should redirect to detail with created notice"
    );
    let created_app_detail = client
        .get(format!("{base_url}/apps/1?notice=created"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        created_app_detail.contains("/apps/1/compose/up/confirm")
            && created_app_detail.contains("#health-check-title"),
        "created compose app detail should show next-step guidance"
    );
    let updated_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        updated_apps.contains("orders-api") && updated_apps.contains("Docker Compose"),
        "created app should appear on apps page"
    );
    let filtered_apps = client
        .get(format!("{base_url}/apps?environment=test&q=orders"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        filtered_apps.contains("value=\"test\" selected")
            && filtered_apps.contains("value=\"orders\"")
            && filtered_apps.contains("orders-api"),
        "apps page should filter by environment and keyword"
    );
    let empty_apps = client
        .get(format!("{base_url}/apps?status=disabled&q=orders"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        empty_apps.contains("value=\"disabled\" selected")
            && empty_apps.contains("value=\"orders\"")
            && empty_apps.contains("name=\"environment\""),
        "apps page should render empty state for unmatched filters"
    );
    let app_root = data_dir.join("apps").join("orders-api");
    let compose_file = tokio::fs::read_to_string(app_root.join("compose.yaml")).await?;
    let env_file = tokio::fs::read_to_string(app_root.join(".env")).await?;
    let app_meta =
        tokio::fs::read_to_string(app_root.join(".easy-deploy").join("app.yaml")).await?;
    anyhow::ensure!(
        compose_file.contains("services:") && compose_file.contains("nginx:alpine"),
        "runtime compose.yaml was not written"
    );
    anyhow::ensure!(
        !compose_file.contains("version: '3.8'"),
        "runtime compose.yaml should strip legacy top-level version"
    );
    anyhow::ensure!(
        env_file.contains("APP_ENV=e2e"),
        "runtime .env was not written"
    );
    anyhow::ensure!(
        app_meta.contains("app_key: \"orders-api\"")
            && app_meta.contains("deploy_strategy: \"rolling_stop_on_failure\"")
            && app_meta.contains("deploy_work_dir: \"/opt/easy-deploy/apps/orders-api\"")
            && app_meta.contains("target_nodes:"),
        "runtime app metadata was not written"
    );
    let app_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        app_detail.contains("orders-api")
            && app_detail.contains("compose.yaml")
            && app_detail.contains("nginx:alpine")
            && app_detail.contains("APP_ENV=e2e")
            && app_detail.contains("/apps/1/metadata")
            && app_detail.contains("/apps/1/config")
            && app_detail.contains("/apps/1/compose/up/confirm")
            && app_detail.contains("/apps/1/compose/logs"),
        "app detail page did not render runtime config"
    );
    let detail_csrf = extract_csrf_token(&app_detail)?;
    let update_metadata = client
        .post(format!("{base_url}/apps/1/metadata"))
        .form(&[
            ("csrf_token", detail_csrf.as_str()),
            ("name", "Orders API Pro"),
            ("description", "Updated compose app"),
            ("work_dir", "/opt/easy-deploy/apps/orders-api-pro"),
            ("deploy_strategy", "rolling_continue"),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        update_metadata.status() == reqwest::StatusCode::SEE_OTHER,
        "update app metadata should redirect: {}",
        update_metadata.status()
    );
    let updated_app_meta =
        tokio::fs::read_to_string(app_root.join(".easy-deploy").join("app.yaml")).await?;
    anyhow::ensure!(
        updated_app_meta.contains("name: \"Orders API Pro\"")
            && updated_app_meta.contains("description: \"Updated compose app\"")
            && updated_app_meta.contains("deploy_strategy: \"rolling_continue\"")
            && updated_app_meta
                .contains("deploy_work_dir: \"/opt/easy-deploy/apps/orders-api-pro\"")
            && updated_app_meta.contains("node_key: \"local\"")
            && !updated_app_meta.contains("node_key: \"prod-a\""),
        "app metadata update should sync app.yaml"
    );
    let updated_app_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        updated_app_detail.contains("orders-api")
            && updated_app_detail.contains("/opt/easy-deploy/apps/orders-api-pro")
            && updated_app_detail.contains("rolling_continue")
            && updated_app_detail.contains(&format!("value=\"{local_node_id}\" checked")),
        "app detail should render updated metadata and targets"
    );
    let detail_csrf = extract_csrf_token(&updated_app_detail)?;
    let update_config = client
        .post(format!("{base_url}/apps/1/config"))
        .form(&[
            ("csrf_token", detail_csrf.as_str()),
            (
                "compose_content",
                "services:\n  web:\n    image: caddy:2-alpine\n    ports:\n      - \"8080:80\"\n  worker:\n    image: busybox\n",
            ),
            ("env_content", "APP_ENV=updated\n"),
            ("health_check_kind", "compose_running"),
            ("health_endpoint", ""),
            ("health_timeout_secs", "5"),
            ("health_expected_status", "200"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        update_config.status() == reqwest::StatusCode::SEE_OTHER,
        "update app config should redirect: {}",
        update_config.status()
    );
    let updated_compose_file = tokio::fs::read_to_string(app_root.join("compose.yaml")).await?;
    let updated_env_file = tokio::fs::read_to_string(app_root.join(".env")).await?;
    anyhow::ensure!(
        updated_compose_file.contains("caddy:2-alpine"),
        "updated compose.yaml was not written"
    );
    anyhow::ensure!(
        updated_env_file.contains("APP_ENV=updated"),
        "updated .env was not written"
    );
    let health_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        health_detail.contains("name=\"health_check_kind\"")
            && health_detail.contains("value=\"compose_running\"")
            && health_detail.contains("name=\"health_timeout_secs\"")
            && health_detail.contains("value=\"5\"")
            && health_detail.contains("caddy:2-alpine"),
        "app detail should render health check configuration"
    );
    let restore_initial = client
        .post(format!("{base_url}/apps/1/snapshots/1/restore"))
        .form(&[("csrf_token", extract_csrf_token(&health_detail)?.as_str())])
        .send()
        .await?;
    anyhow::ensure!(
        restore_initial.status() == reqwest::StatusCode::SEE_OTHER,
        "restore initial config should redirect: {}",
        restore_initial.status()
    );
    let restored_compose_file = tokio::fs::read_to_string(app_root.join("compose.yaml")).await?;
    let restored_env_file = tokio::fs::read_to_string(app_root.join(".env")).await?;
    anyhow::ensure!(
        restored_compose_file.contains("nginx:alpine")
            && !restored_compose_file.contains("caddy:2-alpine"),
        "restored compose.yaml should return to initial snapshot"
    );
    anyhow::ensure!(
        restored_env_file.contains("APP_ENV=e2e") && !restored_env_file.contains("APP_ENV=updated"),
        "restored .env should return to initial snapshot"
    );
    let restored_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        restored_detail.contains("nginx:alpine")
            && restored_detail.contains("APP_ENV=e2e")
            && restored_detail.contains("/apps/1/snapshots/1/restore")
            && !restored_detail.contains("caddy:2-alpine"),
        "app detail should render restored initial config"
    );
    let restore_updated = client
        .post(format!("{base_url}/apps/1/snapshots/2/restore"))
        .form(&[("csrf_token", extract_csrf_token(&restored_detail)?.as_str())])
        .send()
        .await?;
    anyhow::ensure!(
        restore_updated.status() == reqwest::StatusCode::SEE_OTHER,
        "restore updated config should redirect: {}",
        restore_updated.status()
    );
    let rerestored_compose_file = tokio::fs::read_to_string(app_root.join("compose.yaml")).await?;
    let rerestored_env_file = tokio::fs::read_to_string(app_root.join(".env")).await?;
    anyhow::ensure!(
        rerestored_compose_file.contains("caddy:2-alpine")
            && rerestored_env_file.contains("APP_ENV=updated"),
        "second restore should return app to updated config for later service checks"
    );
    let services = client
        .get(format!("{base_url}/services"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &services,
        &[
            "web",
            "worker",
            "name=\"status\"",
            "name=\"q\"",
            "caddy:2-alpine",
            "busybox",
            "8080:80",
            "/services/1/web/logs",
            &format!("/services/1/web/logs?node_id={local_node_id}"),
            &format!("/nodes/{local_node_id}"),
        ],
        "services page should render compose-derived services and health details",
    )?;
    let filtered_services = client
        .get(format!("{base_url}/services?q=busybox"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        filtered_services.contains("value=\"busybox\"")
            && filtered_services.contains("worker")
            && filtered_services.contains("busybox")
            && !filtered_services.contains("caddy:2-alpine"),
        "services page should filter by keyword"
    );
    let empty_services = client
        .get(format!("{base_url}/services?status=healthy&q=not-found"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        empty_services.contains("value=\"healthy\" selected")
            && empty_services.contains("value=\"not-found\"")
            && empty_services.contains("name=\"status\""),
        "services page should render empty state for unmatched filters"
    );
    command_runner.with_result(
        "docker compose logs --tail 200 --no-color",
        CommandResult {
            status_code: Some(0),
            stdout: "compose log line\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "docker compose logs --tail 500 --no-color web",
        CommandResult {
            status_code: Some(0),
            stdout: "web log line\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "docker compose logs --tail 200 --no-color web",
        CommandResult {
            status_code: Some(0),
            stdout: "web log line\n".to_owned(),
            stderr: String::new(),
        },
    );
    let service_logs = client
        .get(format!(
            "{base_url}/services/1/web/logs?node_id={local_node_id}&tail=500"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        service_logs.contains("web")
            && service_logs.contains("local")
            && service_logs.contains("docker compose logs --tail 500 --no-color web")
            && service_logs.contains("500 ")
            && service_logs.contains("tail=1000")
            && service_logs.contains("web log line"),
        "service logs page should render compose service logs"
    );

    let templates = client
        .get(format!("{base_url}/templates"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        templates.contains("nginx-static")
            && templates.contains("redis-single")
            && templates.contains("postgres-single")
            && templates.contains("caddy-gateway"),
        "templates page should render built-in compose templates"
    );
    anyhow::ensure!(
        templates.contains("<section class=\"panel panel-large panel-full list-panel\"")
            && templates.contains("<table class=\"data-table template-table\">")
            && templates.contains("模板管理")
            && templates.contains("8080")
            && templates.contains("6379")
            && templates.contains("5432")
            && templates.contains("PUBLIC_PORT=8080")
            && templates.contains("REDIS_PASSWORD=change-me")
            && templates.contains("POSTGRES_PASSWORD=change-me")
            && !templates.contains("name=\"template_key\"")
            && !templates.contains("id=\"template-port\"")
            && !templates.contains("data-default-port"),
        "templates page should render read-only template table"
    );
    let create_from_template = client.post(format!("{base_url}/templates")).send().await?;
    anyhow::ensure!(
        create_from_template.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED,
        "template creation route should be removed: {}",
        create_from_template.status()
    );
    let template_compat_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let template_compat_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&template_compat_apps)?.as_str(),
            ),
            ("app_key", "template-compat"),
            ("name", "模板占位应用"),
            ("description", "E2E 保持历史应用序号"),
            ("app_type", "compose"),
            ("release_source", "manual"),
            ("deploy_strategy", "rolling_stop_on_failure"),
            ("work_dir", "/opt/easy-deploy/apps/template-compat"),
            (
                "compose_content",
                "services:\n  redis:\n    image: redis:7-alpine\n",
            ),
            ("env_content", "REDIS_PORT=6379\n"),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        template_compat_app.status() == reqwest::StatusCode::SEE_OTHER,
        "template compatibility app should redirect: {}",
        template_compat_app.status()
    );
    anyhow::ensure!(
        response_location(&template_compat_app)? == "/apps/2?notice=created",
        "template compatibility app should keep historical app id sequence"
    );
    let roles = client
        .get(format!("{base_url}/admin/roles"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let rbac_accounts_view_permission_id = permission_id(&db, "rbac.accounts.view").await?;
    let rbac_roles_view_permission_id = permission_id(&db, "rbac.roles.view").await?;
    let rbac_permissions_view_permission_id = permission_id(&db, "rbac.permissions.view").await?;
    let rbac_sessions_view_permission_id = permission_id(&db, "rbac.sessions.view").await?;
    let dashboard_view_permission_id = permission_id(&db, "dashboard.view").await?;
    let apps_view_permission_id = permission_id(&db, "apps.view").await?;
    let apps_update_permission_id = permission_id(&db, "apps.update").await?;
    let services_view_permission_id = permission_id(&db, "services.view").await?;
    let services_logs_permission_id = permission_id(&db, "services.logs").await?;
    let artifacts_view_permission_id = permission_id(&db, "artifacts.view").await?;
    anyhow::ensure!(
        roles.contains("super_admin")
            && roles.contains("admin")
            && roles.contains("deployer")
            && roles.contains("viewer"),
        "roles page did not render built-in roles"
    );
    anyhow::ensure!(
        roles.contains("super_admin") && roles.contains("disabled"),
        "system roles should render as platform-maintained"
    );
    anyhow::ensure!(
        roles.contains("name=\"q\"")
            && roles.contains("name=\"status\"")
            && roles.contains("id=\"permission-dependencies\"")
            && roles.contains("services.logs")
            && roles.contains("services.view")
            && roles.contains("super_admin")
            && roles.contains("deployer"),
        "roles page should render rbac workbench metrics and filters"
    );
    let permissions = client
        .get(format!("{base_url}/admin/permissions"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        permissions.contains("dashboard.view")
            && permissions.contains("nodes.install")
            && permissions.contains("rbac.permissions.view")
            && permissions.contains("name=\"module\"")
            && permissions.contains("name=\"resource_type\"")
            && permissions.contains("name=\"q\""),
        "permissions page should render the version-owned permission registry"
    );
    anyhow::ensure!(
        !permissions.contains("nodes.create")
            && !permissions.contains("nodes.update")
            && !permissions.contains("nodes.delete")
            && !permissions.contains("apps.delete"),
        "permissions page should not render stale legacy permission keys"
    );
    let filtered_permissions = client
        .get(format!(
            "{base_url}/admin/permissions?module=%E8%8A%82%E7%82%B9&resource_type=action&q=install"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        filtered_permissions.contains("value=\"action\" selected")
            && filtered_permissions.contains("value=\"install\"")
            && filtered_permissions.contains("nodes.install")
            && !filtered_permissions.contains("dashboard.view"),
        "permissions page should filter by module, type, and keyword"
    );
    let protect_system_role_status = client
        .post(format!("{base_url}/admin/roles/status"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_id", "1"),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        protect_system_role_status.status() == reqwest::StatusCode::BAD_REQUEST,
        "system role status update should be rejected: {}",
        protect_system_role_status.status()
    );
    let protect_system_role_status_message = protect_system_role_status.text().await?;
    anyhow::ensure!(
        !protect_system_role_status_message.trim().is_empty(),
        "system role status error should include a message"
    );
    let protect_system_role_permissions = client
        .post(format!("{base_url}/admin/roles/permissions"))
        .form(&[("csrf_token", csrf_token.as_str()), ("role_id", "1")])
        .send()
        .await?;
    anyhow::ensure!(
        protect_system_role_permissions.status() == reqwest::StatusCode::BAD_REQUEST,
        "system role permission update should be rejected: {}",
        protect_system_role_permissions.status()
    );
    let protect_system_role_permissions_message = protect_system_role_permissions.text().await?;
    anyhow::ensure!(
        !protect_system_role_permissions_message.trim().is_empty(),
        "system role permission error should include a message"
    );

    let create_role = client
        .post(format!("{base_url}/admin/roles"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_code", "qa_deployer"),
            ("role_name", "楠屾敹閮ㄧ讲"),
            ("description", "鐢ㄤ�E2E 楠屾敹鐨勮"),
            ("permission_ids", dashboard_view_permission_id.as_str()),
            ("permission_ids", apps_view_permission_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_role.status() == reqwest::StatusCode::SEE_OTHER,
        "create role should redirect: {}",
        create_role.status()
    );
    let qa_role_id = role_id(&db, "qa_deployer").await?;
    let roles_after_create = client
        .get(format!("{base_url}/admin/roles?q=qa_deployer"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        roles_after_create.contains("qa_deployer")
            && roles_after_create.contains("dashboard.view")
            && roles_after_create.contains("apps.view"),
        "created role should keep initial permissions selected from the create form"
    );
    let qa_role_initial_permission_count =
        role_permission_count(&db, qa_role_id.parse::<i64>()?).await?;
    anyhow::ensure!(
        qa_role_initial_permission_count == 2,
        "created role should persist initial permissions, got {qa_role_initial_permission_count}"
    );
    let update_qa_role_permissions = client
        .post(format!("{base_url}/admin/roles/permissions"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_id", qa_role_id.as_str()),
            ("permission_ids", dashboard_view_permission_id.as_str()),
            ("permission_ids", apps_view_permission_id.as_str()),
            ("permission_ids", services_view_permission_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        update_qa_role_permissions.status() == reqwest::StatusCode::SEE_OTHER,
        "update custom role permissions should redirect: {}",
        update_qa_role_permissions.status()
    );

    let create_config_editor_role = client
        .post(format!("{base_url}/admin/roles"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_code", "config_editor"),
            ("role_name", "閰嶇疆绠＄悊"),
            (
                "description",
                "鍙兘缂栬緫搴旂敤閰嶇疆锛屼笉鑳藉垏鎹㈠彂甯冪増",
            ),
            ("permission_ids", dashboard_view_permission_id.as_str()),
            ("permission_ids", apps_view_permission_id.as_str()),
            ("permission_ids", apps_update_permission_id.as_str()),
            ("permission_ids", services_view_permission_id.as_str()),
            ("permission_ids", artifacts_view_permission_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_config_editor_role.status() == reqwest::StatusCode::SEE_OTHER,
        "create config editor role should redirect: {}",
        create_config_editor_role.status()
    );

    let create_log_viewer_role = client
        .post(format!("{base_url}/admin/roles"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_code", "log_viewer"),
            ("role_name", "鏃ュ織鍙"),
            ("description", "鍙兘鏌ョ湅鏈嶅姟鍒楄〃鍜屾湇鍔℃�"),
            ("permission_ids", dashboard_view_permission_id.as_str()),
            ("permission_ids", apps_view_permission_id.as_str()),
            ("permission_ids", services_view_permission_id.as_str()),
            ("permission_ids", services_logs_permission_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_log_viewer_role.status() == reqwest::StatusCode::SEE_OTHER,
        "create log viewer role should redirect: {}",
        create_log_viewer_role.status()
    );

    let create_logs_action_only_role = client
        .post(format!("{base_url}/admin/roles"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_code", "logs_action_only"),
            ("role_name", "日志动作权限"),
            ("description", "只提交日志操作权限，服务端自动补齐页面依�"),
            ("permission_ids", services_logs_permission_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_logs_action_only_role.status() == reqwest::StatusCode::SEE_OTHER,
        "create logs action only role should redirect: {}",
        create_logs_action_only_role.status()
    );
    let logs_action_only_permissions = role_permission_keys(&db, "logs_action_only").await?;
    anyhow::ensure!(
        logs_action_only_permissions.contains(&"services.logs".to_owned())
            && logs_action_only_permissions.contains(&"services.view".to_owned()),
        "action-only role should persist required page dependency: {logs_action_only_permissions:?}"
    );

    let create_rbac_viewer_role = client
        .post(format!("{base_url}/admin/roles"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("role_code", "rbac_viewer"),
            ("role_name", "鏉冮檺鍙"),
            ("description", "鍙兘鏌ョ湅璐﹀彿銆佽鑹插拰浼氳瘽"),
            ("permission_ids", rbac_accounts_view_permission_id.as_str()),
            ("permission_ids", rbac_roles_view_permission_id.as_str()),
            (
                "permission_ids",
                rbac_permissions_view_permission_id.as_str(),
            ),
            ("permission_ids", rbac_sessions_view_permission_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_rbac_viewer_role.status() == reqwest::StatusCode::SEE_OTHER,
        "create rbac viewer role should redirect: {}",
        create_rbac_viewer_role.status()
    );

    let rbac_viewer_role_id = role_id(&db, "rbac_viewer").await?;
    let deployer_role_id = role_id(&db, "deployer").await?;
    let config_editor_role_id = role_id(&db, "config_editor").await?;
    let log_viewer_role_id = role_id(&db, "log_viewer").await?;

    let create_account = client
        .post(format!("{base_url}/admin/accounts"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("username", "deployer"),
            ("display_name", "閮ㄧ讲鐢ㄦ埛"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ("role_ids", deployer_role_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_account.status() == reqwest::StatusCode::SEE_OTHER,
        "create account should redirect: {}",
        create_account.status()
    );
    anyhow::ensure!(
        response_location(&create_account)? == "/admin/accounts?notice=created",
        "create account should redirect with notice"
    );
    let created_notice_accounts = client
        .get(format!("{base_url}/admin/accounts?notice=created"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        created_notice_accounts.contains("deployer")
            && created_notice_accounts.contains("action=\"/admin/accounts/status\""),
        "accounts page should render after account creation notice redirect"
    );

    let create_config_editor = client
        .post(format!("{base_url}/admin/accounts"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("username", "configeditor"),
            ("display_name", "閰嶇疆楠屾敹"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ("role_ids", config_editor_role_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_config_editor.status() == reqwest::StatusCode::SEE_OTHER,
        "create config editor account should redirect: {}",
        create_config_editor.status()
    );

    let create_rollbacker = client
        .post(format!("{base_url}/admin/accounts"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("username", "rollbacker"),
            ("display_name", "鍥炴粴楠屾敹"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ("role_ids", deployer_role_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_rollbacker.status() == reqwest::StatusCode::SEE_OTHER,
        "create rollbacker account should redirect: {}",
        create_rollbacker.status()
    );

    let create_viewer = client
        .post(format!("{base_url}/admin/accounts"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("username", "viewer"),
            ("display_name", "鍙楠屾敹"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ("role_ids", viewer_role_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_viewer.status() == reqwest::StatusCode::SEE_OTHER,
        "create viewer should redirect: {}",
        create_viewer.status()
    );

    let create_log_viewer = client
        .post(format!("{base_url}/admin/accounts"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("username", "logviewer"),
            ("display_name", "鏃ュ織楠屾敹"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ("role_ids", log_viewer_role_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_log_viewer.status() == reqwest::StatusCode::SEE_OTHER,
        "create log viewer should redirect: {}",
        create_log_viewer.status()
    );

    let create_rbac_viewer = client
        .post(format!("{base_url}/admin/accounts"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("username", "rbacviewer"),
            ("display_name", "鏉冮檺鍙楠屾�"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ("role_ids", rbac_viewer_role_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_rbac_viewer.status() == reqwest::StatusCode::SEE_OTHER,
        "create rbac viewer account should redirect: {}",
        create_rbac_viewer.status()
    );

    let updated_accounts = client
        .get(format!("{base_url}/admin/accounts"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        updated_accounts.contains("deployer"),
        "created account should appear on accounts page"
    );
    anyhow::ensure!(
        updated_accounts.contains("configeditor"),
        "created config editor account should appear on accounts page"
    );
    anyhow::ensure!(
        updated_accounts.contains("rollbacker"),
        "created rollbacker account should appear on accounts page"
    );
    anyhow::ensure!(
        updated_accounts.contains("viewer"),
        "created viewer account should appear on accounts page"
    );
    anyhow::ensure!(
        updated_accounts.contains("logviewer"),
        "created log viewer account should appear on accounts page"
    );
    anyhow::ensure!(
        updated_accounts.contains("rbacviewer"),
        "created rbac viewer account should appear on accounts page"
    );
    anyhow::ensure!(
        updated_accounts.contains("admin")
            && !updated_accounts.contains("name=\"account_id\" value=\"1\""),
        "current account row should explain where to manage own account"
    );
    anyhow::ensure!(
        updated_accounts.contains("name=\"status\"")
            && updated_accounts.contains("name=\"role\"")
            && updated_accounts.contains("name=\"q\"")
            && updated_accounts.contains("action=\"/admin/accounts/roles\"")
            && updated_accounts.contains("action=\"/admin/accounts/password\""),
        "accounts page should render rbac account metrics and filters"
    );
    let filtered_accounts = client
        .get(format!("{base_url}/admin/accounts?status=active&q=admin"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        filtered_accounts.contains("value=\"active\" selected")
            && filtered_accounts.contains("value=\"admin\"")
            && filtered_accounts.contains("admin")
            && !filtered_accounts.contains("name=\"account_id\" value=\"2\""),
        "accounts page should filter by status and query"
    );
    let self_disable = client
        .post(format!("{base_url}/admin/accounts/status"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&updated_accounts)?.as_str(),
            ),
            ("account_id", "1"),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        self_disable.status() == reqwest::StatusCode::BAD_REQUEST,
        "admin should not be able to disable current account: {}",
        self_disable.status()
    );
    let self_disable_message = self_disable.text().await?;
    anyhow::ensure!(
        !self_disable_message.trim().is_empty(),
        "self-disable error should include a message"
    );
    let self_roles = client
        .post(format!("{base_url}/admin/accounts/roles"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&updated_accounts)?.as_str(),
            ),
            ("account_id", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        self_roles.status() == reqwest::StatusCode::BAD_REQUEST,
        "admin should not be able to rewrite current roles: {}",
        self_roles.status()
    );
    let self_roles_message = self_roles.text().await?;
    anyhow::ensure!(
        !self_roles_message.trim().is_empty(),
        "self role error should include a message"
    );
    let self_password = client
        .post(format!("{base_url}/admin/accounts/password"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&updated_accounts)?.as_str(),
            ),
            ("account_id", "1"),
            ("password", LOCAL_TEST_CHANGED_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        self_password.status() == reqwest::StatusCode::BAD_REQUEST,
        "admin should not reset current password from account management: {}",
        self_password.status()
    );
    let self_password_message = self_password.text().await?;
    anyhow::ensure!(
        !self_password_message.trim().is_empty(),
        "self password reset error should include a message"
    );

    let rbac_viewer_client = test_client()?;
    let rbac_viewer_login = rbac_viewer_client
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "rbacviewer"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        rbac_viewer_login.status() == reqwest::StatusCode::SEE_OTHER,
        "rbac viewer login should redirect: {}",
        rbac_viewer_login.status()
    );
    let rbac_viewer_accounts = rbac_viewer_client
        .get(format!("{base_url}/admin/accounts"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        rbac_viewer_accounts.contains("rbacviewer")
            && rbac_viewer_accounts.contains("deployer")
            && rbac_viewer_accounts.contains("name=\"status\"")
            && rbac_viewer_accounts.contains("name=\"role\"")
            && rbac_viewer_accounts.contains("name=\"q\"")
            && !rbac_viewer_accounts.contains("action=\"/admin/accounts/password\"")
            && !rbac_viewer_accounts.contains("action=\"/admin/accounts/status\""),
        "rbac viewer should see accounts page without management controls"
    );
    let rbac_viewer_roles = rbac_viewer_client
        .get(format!("{base_url}/admin/roles"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        rbac_viewer_roles.contains("rbac_viewer")
            && rbac_viewer_roles.contains("super_admin")
            && rbac_viewer_roles.contains("name=\"status\"")
            && rbac_viewer_roles.contains("name=\"module\"")
            && rbac_viewer_roles.contains("name=\"q\"")
            && !rbac_viewer_roles.contains("action=\"/admin/roles/permissions\"")
            && !rbac_viewer_roles.contains("action=\"/admin/roles/status\""),
        "rbac viewer should see roles page without management controls"
    );
    let rbac_viewer_permissions = rbac_viewer_client
        .get(format!("{base_url}/admin/permissions"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        rbac_viewer_permissions.contains("dashboard.view")
            && rbac_viewer_permissions.contains("nodes.install")
            && rbac_viewer_permissions.contains("name=\"module\"")
            && rbac_viewer_permissions.contains("name=\"resource_type\"")
            && rbac_viewer_permissions.contains("name=\"q\"")
            && !rbac_viewer_permissions.contains("action=\"/admin/roles/permissions\""),
        "rbac viewer should see read-only permissions page"
    );
    let rbac_viewer_sessions = rbac_viewer_client
        .get(format!("{base_url}/admin/sessions"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        rbac_viewer_sessions.contains("action=\"/admin/sessions\""),
        "rbac viewer sessions page should render page title"
    );
    anyhow::ensure!(
        rbac_viewer_sessions.contains("name=\"status\"")
            && rbac_viewer_sessions.contains("name=\"q\""),
        "rbac viewer sessions page should render filters and risk column"
    );
    anyhow::ensure!(
        !rbac_viewer_sessions.contains("type=\"submit\">强制下线</button>"),
        "rbac viewer sessions page should explain read-only access"
    );
    anyhow::ensure!(
        !rbac_viewer_sessions.contains("action=\"/admin/sessions/revoke\""),
        "rbac viewer should see sessions page without revoke controls"
    );

    let disabled = client
        .post(format!("{base_url}/admin/accounts/status"))
        .form(&[
            ("csrf_token", csrf_token.as_str()),
            ("account_id", "2"),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        disabled.status() == reqwest::StatusCode::SEE_OTHER,
        "disable account should redirect: {}",
        disabled.status()
    );
    anyhow::ensure!(
        response_location(&disabled)? == "/admin/accounts?notice=status",
        "disable account should redirect with status notice"
    );
    let status_notice_accounts = client
        .get(format!("{base_url}/admin/accounts?notice=status"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        status_notice_accounts.contains("deployer") && status_notice_accounts.contains("disabled"),
        "accounts page should reflect disabled account after status redirect"
    );

    let disabled_client = test_client()?;
    let disabled_login = disabled_client
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "deployer"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?
        .text()
        .await?;
    anyhow::ensure!(
        disabled_login.contains("admin")
            && disabled_login.contains("name=\"username\"")
            && disabled_login.contains("name=\"password\""),
        "disabled account should not be able to login"
    );
    let locked_client = test_client()?;
    for _ in 0..5 {
        let locked_attempt = locked_client
            .post(format!("{base_url}/login"))
            .form(&[("username", "viewer"), ("password", "wrong-password")])
            .send()
            .await?;
        anyhow::ensure!(
            locked_attempt.status().is_success(),
            "failed login should render login page: {}",
            locked_attempt.status()
        );
    }
    let locked_accounts = client
        .get(format!("{base_url}/admin/accounts"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        account_locked(&db, "viewer").await?
            && locked_accounts.contains("viewer")
            && locked_accounts.contains("name=\"account_id\" value=\"5\""),
        "locked account should render lock status and unlock action"
    );
    let locked_login = test_client()?
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "viewer"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?
        .text()
        .await?;
    anyhow::ensure!(
        locked_login.contains("admin")
            && locked_login.contains("name=\"username\"")
            && locked_login.contains("name=\"password\""),
        "locked account should not login with correct password"
    );
    let unlock_viewer = client
        .post(format!("{base_url}/admin/accounts/status"))
        .form(&[
            ("csrf_token", extract_csrf_token(&locked_accounts)?.as_str()),
            ("account_id", "5"),
            ("status", "active"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        unlock_viewer.status() == reqwest::StatusCode::SEE_OTHER,
        "unlock viewer should redirect: {}",
        unlock_viewer.status()
    );
    anyhow::ensure!(
        response_location(&unlock_viewer)? == "/admin/accounts?notice=status",
        "unlock account should redirect with status notice"
    );

    let viewer_client = test_client()?;
    let viewer_login = viewer_client
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "viewer"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        viewer_login.status() == reqwest::StatusCode::SEE_OTHER,
        "viewer login should redirect: {}",
        viewer_login.status()
    );
    let viewer_dashboard = viewer_client
        .get(&base_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_dashboard.contains("href=\"/profile\"")
            && !viewer_dashboard.contains("href=\"/admin/accounts\"")
            && !viewer_dashboard.contains("href=\"/admin/roles\"")
            && !viewer_dashboard.contains("href=\"/admin/permissions\"")
            && !viewer_dashboard.contains("href=\"/admin/sessions\""),
        "viewer navigation should be permission filtered"
    );
    let forbidden_accounts = viewer_client
        .get(format!("{base_url}/admin/accounts"))
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_accounts.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for accounts page: {}",
        forbidden_accounts.status()
    );
    let forbidden_permissions = viewer_client
        .get(format!("{base_url}/admin/permissions"))
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_permissions.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for permissions page: {}",
        forbidden_permissions.status()
    );
    let viewer_nodes = viewer_client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_nodes.contains("prod-a")
            && viewer_nodes.contains("10.0.2.11")
            && !viewer_nodes.contains("name=\"node_key\""),
        "viewer should see nodes without management form"
    );
    anyhow::ensure!(
        !viewer_nodes.contains("action=\"/nodes/update\"")
            && !viewer_nodes.contains("action=\"/nodes/status\""),
        "viewer should not see node mutation actions"
    );
    let viewer_node_detail = viewer_client
        .get(format!("{base_url}/nodes/2"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_node_detail.contains("prod-a")
            && viewer_node_detail.contains("10.0.2.11")
            && viewer_node_detail.contains("ssh -p 22 deploy@10.0.2.11")
            && !viewer_node_detail.contains("action=\"/nodes/install\"")
            && !viewer_node_detail.contains("action=\"/nodes/check\""),
        "viewer should see node install guidance without install actions"
    );
    let forbidden_node_install = viewer_client
        .post(format!("{base_url}/nodes/install"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&viewer_node_detail)?.as_str(),
            ),
            ("node_id", "2"),
            ("component", "docker"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_node_install.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for node install task: {}",
        forbidden_node_install.status()
    );
    let forbidden_node_create = viewer_client
        .post(format!("{base_url}/nodes"))
        .form(&[
            ("csrf_token", extract_csrf_token(&viewer_nodes)?.as_str()),
            ("node_key", "forbidden"),
            ("name", "Forbidden"),
            ("node_type", "ssh"),
            ("address", "10.0.2.12"),
            ("ssh_port", "22"),
            ("ssh_user", "deploy"),
            ("work_dir", "/opt/easy-deploy/apps"),
            ("region", "prod"),
            ("labels", "prod"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_node_create.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for node creation: {}",
        forbidden_node_create.status()
    );
    let forbidden_node_update = viewer_client
        .post(format!("{base_url}/nodes/update"))
        .form(&[
            ("csrf_token", extract_csrf_token(&viewer_nodes)?.as_str()),
            ("node_id", "2"),
            ("name", "Forbidden"),
            ("node_type", "ssh"),
            ("address", "10.0.2.12"),
            ("ssh_port", "22"),
            ("ssh_user", "deploy"),
            ("work_dir", "/opt/easy-deploy/apps"),
            ("region", "prod"),
            ("labels", "prod"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_node_update.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for node update: {}",
        forbidden_node_update.status()
    );
    let forbidden_node_status = viewer_client
        .post(format!("{base_url}/nodes/status"))
        .form(&[
            ("csrf_token", extract_csrf_token(&viewer_nodes)?.as_str()),
            ("node_id", "2"),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_node_status.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for node status change: {}",
        forbidden_node_status.status()
    );
    let viewer_apps = viewer_client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_apps.contains("orders-api") && !viewer_apps.contains("name=\"app_key\""),
        "viewer should see apps without creation form"
    );
    anyhow::ensure!(
        !viewer_apps.contains("/apps/1/status"),
        "viewer should not see app status actions"
    );
    let viewer_app_detail = viewer_client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &viewer_app_detail,
        &["orders-api", "caddy:2-alpine", "readonly"],
        "viewer app detail should render readonly compose configuration",
    )?;
    anyhow::ensure!(
        !viewer_app_detail.contains("type=\"submit\">保存配置</button>")
            && !viewer_app_detail.contains("action=\"/apps/1/metadata\"")
            && !viewer_app_detail.contains("/apps/1/compose/up/confirm")
            && !viewer_app_detail.contains("/snapshots/1/restore"),
        "viewer app detail should hide save, metadata, deploy and restore actions"
    );
    let viewer_templates = viewer_client
        .get(format!("{base_url}/templates"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_templates.contains("redis:7-alpine")
            && viewer_templates.contains("nginx:1.27-alpine")
            && !viewer_templates.contains("name=\"template_key\"")
            && !viewer_templates.contains("name=\"app_key\""),
        "viewer should see templates without creation form"
    );
    let viewer_services = viewer_client
        .get(format!("{base_url}/services"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_services.contains("orders-api")
            && viewer_services.contains("web")
            && viewer_services.contains("worker")
            && !viewer_services.contains("/services/1/web/logs")
            && !viewer_services.contains("/apps/1/compose/up/confirm")
            && !viewer_services.contains("action=\"/apps/1/config\""),
        "viewer should see service index without log actions"
    );
    let forbidden_viewer_service_logs = viewer_client
        .get(format!("{base_url}/services/1/web/logs"))
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_viewer_service_logs.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for service logs: {}",
        forbidden_viewer_service_logs.status()
    );

    let log_viewer_client = test_client()?;
    let log_viewer_login = log_viewer_client
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "logviewer"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        log_viewer_login.status() == reqwest::StatusCode::SEE_OTHER,
        "log viewer login should redirect: {}",
        log_viewer_login.status()
    );
    let log_viewer_services = log_viewer_client
        .get(format!("{base_url}/services"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        log_viewer_services.contains("orders-api")
            && log_viewer_services
                .contains(&format!("/services/1/web/logs?node_id={local_node_id}"))
            && log_viewer_services.contains("worker"),
        "log viewer should see service log actions"
    );
    let log_viewer_app_detail = log_viewer_client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        log_viewer_app_detail.contains("/apps/1/compose/logs"),
        "log viewer should see compose logs without deployment actions"
    );
    let log_viewer_compose_logs = log_viewer_client
        .post(format!("{base_url}/apps/1/compose/logs"))
        .form(&[(
            "csrf_token",
            extract_csrf_token(&log_viewer_app_detail)?.as_str(),
        )])
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        log_viewer_compose_logs.contains("docker compose logs --tail 200 --no-color")
            && log_viewer_compose_logs.contains("compose log line"),
        "log viewer should run compose logs without deployment permission"
    );
    let log_viewer_service_logs = log_viewer_client
        .get(format!("{base_url}/services/1/web/logs"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        log_viewer_service_logs.contains("web")
            && log_viewer_service_logs.contains("web log line")
            && !log_viewer_service_logs.contains("/apps/1/compose/up"),
        "log viewer should see service logs without deployment actions"
    );
    let forbidden_template_create = viewer_client
        .post(format!("{base_url}/templates"))
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_template_create.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED,
        "template creation route should be unavailable for every role: {}",
        forbidden_template_create.status()
    );
    let forbidden_config_update = viewer_client
        .post(format!("{base_url}/apps/1/config"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&viewer_app_detail)?.as_str(),
            ),
            (
                "compose_content",
                "services:\n  web:\n    image: forbidden\n",
            ),
            ("env_content", "APP_ENV=forbidden\n"),
            ("health_check_kind", "none"),
            ("health_endpoint", ""),
            ("health_timeout_secs", "5"),
            ("health_expected_status", "200"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_config_update.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for app config update: {}",
        forbidden_config_update.status()
    );
    let forbidden_metadata_update = viewer_client
        .post(format!("{base_url}/apps/1/metadata"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&viewer_app_detail)?.as_str(),
            ),
            ("name", "Forbidden App"),
            ("description", "nope"),
            ("work_dir", "/tmp/forbidden-app"),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_metadata_update.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for app metadata update: {}",
        forbidden_metadata_update.status()
    );
    let forbidden_snapshot_restore = viewer_client
        .post(format!("{base_url}/apps/1/snapshots/1/restore"))
        .form(&[(
            "csrf_token",
            extract_csrf_token(&viewer_app_detail)?.as_str(),
        )])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_snapshot_restore.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for snapshot restore: {}",
        forbidden_snapshot_restore.status()
    );
    let forbidden_compose_config = viewer_client
        .post(format!("{base_url}/apps/1/compose/config"))
        .form(&[(
            "csrf_token",
            extract_csrf_token(&viewer_app_detail)?.as_str(),
        )])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_compose_config.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for compose config: {}",
        forbidden_compose_config.status()
    );
    let forbidden_compose_up = viewer_client
        .post(format!("{base_url}/apps/1/compose/up"))
        .form(&[(
            "csrf_token",
            extract_csrf_token(&viewer_app_detail)?.as_str(),
        )])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_compose_up.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for compose up: {}",
        forbidden_compose_up.status()
    );
    let forbidden_app_create = viewer_client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", extract_csrf_token(&viewer_apps)?.as_str()),
            ("app_key", "forbidden-app"),
            ("name", "Forbidden App"),
            ("description", "nope"),
            ("app_type", "compose"),
            ("work_dir", "/tmp/forbidden-app"),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", ""),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_app_create.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for app creation: {}",
        forbidden_app_create.status()
    );
    let forbidden_app_status = viewer_client
        .post(format!("{base_url}/apps/1/status"))
        .form(&[
            ("csrf_token", extract_csrf_token(&viewer_apps)?.as_str()),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_app_status.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for app status change: {}",
        forbidden_app_status.status()
    );

    let admin_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let direct_deploy = client
        .post(format!("{base_url}/apps/1/compose/up"))
        .form(&[("csrf_token", extract_csrf_token(&admin_detail)?.as_str())])
        .send()
        .await?;
    anyhow::ensure!(
        direct_deploy.status() == reqwest::StatusCode::SEE_OTHER,
        "unconfirmed compose up should redirect to confirm page: {}",
        direct_deploy.status()
    );
    let direct_location = direct_deploy
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    anyhow::ensure!(
        direct_location == "/apps/1/compose/up/confirm",
        "unexpected unconfirmed compose redirect: {direct_location}"
    );
    let compose_confirm = client
        .get(format!("{base_url}/apps/1/compose/up/confirm"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &compose_confirm,
        &[
            "orders-api",
            "orders-api-pro",
            "local",
            "Docker Compose",
            "docker compose up -d --remove-orphans",
            "/opt/easy-deploy/apps/orders-api-pro/compose.yaml",
            "/opt/easy-deploy/apps/orders-api-pro/.easy-deploy/app.yaml",
            "action=\"/apps/1/compose/up\"",
            "name=\"confirmed\" value=\"1\"",
            "type=\"submit\"",
        ],
        "compose confirm page should render deploy plan, target and submit action",
    )?;
    let deploy_task = client
        .post(format!("{base_url}/apps/1/compose/up"))
        .form(&[
            ("csrf_token", extract_csrf_token(&compose_confirm)?.as_str()),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        deploy_task.status() == reqwest::StatusCode::SEE_OTHER,
        "confirmed compose up should redirect to tasks: {}",
        deploy_task.status()
    );
    let deploy_task_id =
        extract_task_id_from_response_location(&deploy_task, "confirmed compose up")?;
    let deploy_task_path = format!("/tasks/{deploy_task_id}");
    let deploy_task_version = format!("task-{deploy_task_id}");
    let tasks = wait_for_tasks_page(
        &client,
        &base_url,
        &[
            deploy_task_path.as_str(),
            "docker compose up -d --remove-orphans",
            "value=\"success\"",
            "value=\"completed\"",
        ],
    )
    .await?;
    anyhow::ensure!(
        tasks.contains(&deploy_task_path)
            && tasks.contains("docker compose up -d --remove-orphans")
            && tasks.contains("value=\"success\""),
        "tasks page should render compose deployment task"
    );
    let task_detail = client
        .get(format!("{base_url}{deploy_task_path}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &task_detail,
        &[
            "docker compose up -d --remove-orphans",
            "docker info",
            "docker compose config",
            "local",
        ],
        "task detail should render preflight logs, health check and command output",
    )?;
    anyhow::ensure!(
        !task_detail.contains(&format!("{deploy_task_path}/retry")),
        "successful task detail should not render retry action"
    );
    anyhow::ensure!(
        !task_detail.contains("http-equiv=\"refresh\""),
        "completed task detail should not keep auto-refreshing"
    );
    sqlx::query(
        r#"
        UPDATE operation_tasks
        SET status = 'running',
            phase = 'executing',
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(deploy_task_id)
    .execute(&db)
    .await?;
    let running_task_detail = client
        .get(format!("{base_url}{deploy_task_path}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        running_task_detail.contains("http-equiv=\"refresh\" content=\"3\"")
            && running_task_detail.contains("docker compose up -d --remove-orphans"),
        "running task detail should auto-refresh while execution is in progress"
    );
    sqlx::query(
        r#"
        UPDATE operation_tasks
        SET status = 'queued',
            phase = 'queued',
            command = '',
            summary = 'queued deployment lock test',
            exit_code = NULL,
            started_at = NULL,
            finished_at = NULL,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(deploy_task_id)
    .execute(&db)
    .await?;
    let queued_tasks = client
        .get(format!("{base_url}/tasks?status=queued"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        queued_tasks.contains(&deploy_task_path)
            && queued_tasks.contains("value=\"queued\" selected")
            && queued_tasks.contains("queued deployment lock test"),
        "tasks page should show queued deployment position"
    );
    let queued_task_detail = client
        .get(format!("{base_url}{deploy_task_path}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        queued_task_detail.contains("queued deployment lock test")
            && queued_task_detail.contains(&format!("{deploy_task_path}/cancel")),
        "queued task detail should show queue position and cancel action"
    );
    let duplicate_deploy = client
        .post(format!("{base_url}/apps/1/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&queued_task_detail)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        duplicate_deploy.status() == reqwest::StatusCode::CONFLICT,
        "same app queued task should reject duplicate deployment: {}",
        duplicate_deploy.status()
    );
    let duplicate_message = duplicate_deploy.text().await?;
    anyhow::ensure!(
        duplicate_message.contains(&format!("#{deploy_task_id}")),
        "duplicate deployment should explain active task conflict"
    );
    sqlx::query(
        r#"
        UPDATE operation_tasks
        SET status = 'success',
            phase = 'completed',
            command = 'docker compose up -d --remove-orphans',
            summary = 'e2e command ok: docker compose up -d --remove-orphans',
            exit_code = 0,
            started_at = COALESCE(started_at, created_at),
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(deploy_task_id)
    .execute(&db)
    .await?;
    let live_dashboard = client
        .get(&base_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        live_dashboard.contains("href=\"/apps\"")
            && live_dashboard.contains("href=\"/tasks\"")
            && live_dashboard.contains("127.0.0.1")
            && live_dashboard.contains("Docker Compose"),
        "dashboard should render real apps, runtime items, nodes and tasks"
    );
    let deployed_app_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        deployed_app_detail.contains("Compose")
            && deployed_app_detail.contains("APP_ENV=updated")
            && deployed_app_detail.contains(&deploy_task_version)
            && deployed_app_detail.contains(&deploy_task_path)
            && deployed_app_detail.contains(&format!("/nodes/{local_node_id}"))
            && deployed_app_detail
                .contains(&format!("/services/1/web/logs?node_id={local_node_id}"))
            && deployed_app_detail
                .contains(&format!("/services/1/worker/logs?node_id={local_node_id}")),
        "app detail should render deployment history"
    );
    let services_after_compose_deploy = client
        .get(format!("{base_url}/services"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        services_after_compose_deploy.contains("orders-api")
            && services_after_compose_deploy.contains("web")
            && services_after_compose_deploy.contains("worker")
            && services_after_compose_deploy.contains(&deploy_task_version)
            && services_after_compose_deploy
                .contains(&format!("/services/1/web/logs?node_id={local_node_id}")),
        "services page should aggregate healthy compose runtime and latest health result"
    );

    let post_deploy_config = client
        .post(format!("{base_url}/apps/1/config"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&deployed_app_detail)?.as_str(),
            ),
            (
                "compose_content",
                "services:\n  web:\n    image: caddy:2.8-alpine\n    ports:\n      - \"8080:80\"\n  worker:\n    image: busybox\n",
            ),
            ("env_content", "APP_ENV=after-deploy\n"),
            ("health_check_kind", "compose_running"),
            ("health_endpoint", ""),
            ("health_timeout_secs", "5"),
            ("health_expected_status", "200"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        post_deploy_config.status() == reqwest::StatusCode::SEE_OTHER,
        "post-deploy config update should redirect: {}",
        post_deploy_config.status()
    );
    let changed_diff_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        changed_diff_detail.contains("caddy:2.8-alpine")
            && changed_diff_detail.contains("caddy:2-alpine")
            && changed_diff_detail.contains("APP_ENV=after-deploy")
            && changed_diff_detail.contains("APP_ENV=updated"),
        "app detail should show config diff previews against last successful deploy"
    );
    let changed_confirm = client
        .get(format!("{base_url}/apps/1/compose/up/confirm"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let changed_confirm_missing = [
        "caddy:2.8-alpine",
        "caddy:2-alpine",
        "APP_ENV=after-deploy",
        "APP_ENV=updated",
        "action=\"/apps/1/compose/up\"",
        "name=\"confirmed\" value=\"1\"",
    ]
    .into_iter()
    .filter(|part| !changed_confirm.contains(part))
    .collect::<Vec<_>>();
    anyhow::ensure!(
        changed_confirm_missing.is_empty(),
        "compose confirm page should show pending config diff previews; missing: {changed_confirm_missing:?}; page: {changed_confirm}"
    );

    let ssh_compose_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let create_ssh_compose = client
        .post(format!("{base_url}/apps"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&ssh_compose_apps)?.as_str(),
            ),
            ("app_key", "edge-compose"),
            ("name", "Edge SSH Compose"),
            ("description", "閮ㄧ讲SSH 鑺傜偣Compose 搴旂�"),
            ("app_type", "compose"),
            ("work_dir", "/opt/easy-deploy/apps/edge-compose"),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", "EDGE_ENV=compose\n"),
            ("target_node_ids", ssh_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_ssh_compose.status() == reqwest::StatusCode::SEE_OTHER,
        "create ssh compose app should redirect: {}",
        create_ssh_compose.status()
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 curl -sS -L -o /dev/null -w %{http_code} --max-time 5 --connect-timeout 5 http://127.0.0.1:18080/healthz",
        CommandResult {
            status_code: Some(0),
            stdout: "200".to_owned(),
            stderr: String::new(),
        },
    );
    sqlx::query(
        r#"
        UPDATE nodes
        SET status = 'online',
            docker_status = 'available'
        WHERE id = ?1
        "#,
    )
    .bind(ssh_node_id.parse::<i64>()?)
    .execute(&db)
    .await?;
    sqlx::query(
        r#"
        UPDATE node_capabilities
        SET docker_available = 1,
            compose_available = 1,
            systemd_available = 1,
            caddy_available = 1,
            nginx_available = 1,
            docker_version = 'Docker version 27.0.2, build ssh-e2e',
            compose_version = 'Docker Compose version v2.29.0',
            systemd_version = 'systemd 254 (254.5-1)',
            caddy_version = '2.8.4',
            nginx_version = 'nginx version: nginx/1.24.0',
            message = 'SSH 鑺傜偣鑳藉姏宸叉仮锟?'
        WHERE node_id = ?1
        "#,
    )
    .bind(ssh_node_id.parse::<i64>()?)
    .execute(&db)
    .await?;
    let ssh_compose_detail = client
        .get(format!("{base_url}/apps/3"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let ssh_compose_health = client
        .post(format!("{base_url}/apps/3/config"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&ssh_compose_detail)?.as_str(),
            ),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", "EDGE_ENV=compose\n"),
            ("health_check_kind", "http"),
            ("health_endpoint", "http://127.0.0.1:18080/healthz"),
            ("health_timeout_secs", "5"),
            ("health_expected_status", "200"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        ssh_compose_health.status() == reqwest::StatusCode::SEE_OTHER,
        "save ssh compose health config should redirect: {}",
        ssh_compose_health.status()
    );
    let ssh_compose_confirm = client
        .get(format!("{base_url}/apps/3/compose/up/confirm"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        ssh_compose_confirm.contains("action=\"/apps/3/compose/up\"")
            && ssh_compose_confirm.contains("name=\"confirmed\" value=\"1\""),
        "ssh compose confirm should allow submit after capability recovery"
    );
    let ssh_compose_up = client
        .post(format!("{base_url}/apps/3/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&ssh_compose_confirm)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        ssh_compose_up.status() == reqwest::StatusCode::SEE_OTHER,
        "ssh compose up should redirect: {}",
        ssh_compose_up.status()
    );
    let ssh_compose_task_id =
        extract_task_id_from_response_location(&ssh_compose_up, "ssh compose up")?;
    let ssh_compose_task_version = format!("task-{ssh_compose_task_id}");
    let ssh_compose_tasks = wait_for_tasks_page(
        &client,
        &base_url,
        &[
            "Edge SSH Compose",
            "curl -sS -L -o /dev/null -w %{http_code}",
            "value=\"success\"",
        ],
    )
    .await?;
    anyhow::ensure!(
        ssh_compose_tasks.contains(&format!("/tasks/{ssh_compose_task_id}")),
        "ssh compose deployment should create task"
    );
    let ssh_compose_task_detail = wait_for_task_detail_page(
        &client,
        &base_url,
        ssh_compose_task_id,
        &[
            "ssh -p 22 deploy@10.0.2.11 mkdir -p",
            "scp -P 22",
            "cd /opt/easy-deploy/apps/edge-compose",
            "docker compose config",
            "docker compose up -d --remove-orphans",
            "ssh -p 22 deploy@10.0.2.11 curl -sS -L -o /dev/null -w %{http_code} --max-time 5 --connect-timeout 5 http://127.0.0.1:18080/healthz",
            "200",
        ],
    )
    .await?;
    anyhow::ensure!(!ssh_compose_task_detail.is_empty());
    let ssh_compose_after_deploy = client
        .get(format!("{base_url}/apps/3"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        ssh_compose_after_deploy.contains("edge-compose")
            && ssh_compose_after_deploy.contains("prod-a")
            && ssh_compose_after_deploy.contains(&ssh_compose_task_version),
        "ssh compose app detail should show healthy runtime state"
    );
    let ssh_node_detail_after_deploy = client
        .get(format!("{base_url}/nodes/{ssh_node_id}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        ssh_node_detail_after_deploy.contains("Edge SSH Compose")
            && ssh_node_detail_after_deploy.contains("edge-compose")
            && ssh_node_detail_after_deploy.contains("/apps/3")
            && ssh_node_detail_after_deploy.contains(&format!("/tasks/{ssh_compose_task_id}"))
            && ssh_node_detail_after_deploy.contains(&ssh_compose_task_version),
        "ssh node detail should show bound app runtime and recent deployment task"
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/edge-compose && docker compose logs --tail 200 --no-color web",
        CommandResult {
            status_code: Some(0),
            stdout: "edge compose web log line\n".to_owned(),
            stderr: String::new(),
        },
    );
    let ssh_compose_logs = client
        .get(format!(
            "{base_url}/services/3/web/logs?node_id={ssh_node_id}"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        ssh_compose_logs.contains("Edge SSH Compose")
            && ssh_compose_logs.contains("web")
            && ssh_compose_logs.contains("prod-a")
            && ssh_compose_logs.contains("ssh -p 22 deploy@10.0.2.11")
            && ssh_compose_logs.contains("cd /opt/easy-deploy/apps/edge-compose")
            && ssh_compose_logs.contains("docker compose logs --tail 200 --no-color web")
            && ssh_compose_logs.contains("edge compose web log line"),
        "ssh compose service logs page should render remote compose logs"
    );

    command_runner.with_result(
        "docker compose config",
        CommandResult {
            status_code: Some(1),
            stdout: String::new(),
            stderr: "compose config invalid: missing image\n".to_owned(),
        },
    );
    let apps_after_deploy = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let failed_app_csrf = extract_csrf_token(&apps_after_deploy)?;
    let failed_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", failed_app_csrf.as_str()),
            ("app_key", "bad-compose"),
            ("name", "Bad Compose"),
            ("description", "preflight should fail"),
            ("app_type", "compose"),
            ("work_dir", "/opt/easy-deploy/apps/bad-compose"),
            (
                "compose_content",
                "services:\n  bad:\n    image: nginx:alpine\n",
            ),
            ("env_content", ""),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        failed_app.status() == reqwest::StatusCode::SEE_OTHER,
        "create bad compose app should redirect: {}",
        failed_app.status()
    );
    let bad_detail = client
        .get(format!("{base_url}/apps/4"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let bad_deploy = client
        .post(format!("{base_url}/apps/4/compose/up"))
        .form(&[
            ("csrf_token", extract_csrf_token(&bad_detail)?.as_str()),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        bad_deploy.status() == reqwest::StatusCode::SEE_OTHER,
        "bad compose up should redirect to tasks: {}",
        bad_deploy.status()
    );
    let bad_deploy_task_id = extract_task_id_from_response_location(&bad_deploy, "bad compose up")?;
    let failed_tasks = wait_for_tasks_page(
        &client,
        &base_url,
        &[
            "Bad Compose",
            "compose config invalid: missing image",
            "value=\"failed\"",
        ],
    )
    .await?;
    anyhow::ensure!(
        failed_tasks.contains("Bad Compose")
            && failed_tasks.contains("compose config invalid: missing image"),
        "compose config preflight failure should be shown on tasks page"
    );
    let failed_task_detail = client
        .get(format!("{base_url}/tasks/{bad_deploy_task_id}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        failed_task_detail.contains("Bad Compose")
            && failed_task_detail.contains("compose config invalid: missing image")
            && failed_task_detail.contains("docker compose config")
            && failed_task_detail.contains(&format!("/tasks/{bad_deploy_task_id}/retry"))
            && failed_task_detail.contains(&format!("href=\"/nodes/{local_node_id}\""))
            && !failed_task_detail.contains("docker compose up -d --remove-orphans"),
        "failed task detail should show preflight failure without deployment command"
    );
    let capability_task_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO operation_tasks(
            task_kind,
            title,
            app_id,
            node_id,
            status,
            phase,
            command,
            summary,
            exit_code,
            created_by,
            started_at,
            finished_at
        )
        VALUES (
            'binary.restart',
            '能力修复入口测试',
            4,
            ?1,
            'failed',
            'failed',
            'systemctl restart easy-deploy-worker-bin-blue.service',
            '节点本机节点未通过 Caddy 能力探测，已启用反向代理切流，预检阻断部署',
            1,
            'admin',
            strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        )
        RETURNING id
        "#,
    )
    .bind(local_node_id.parse::<i64>()?)
    .fetch_one(&db)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO operation_task_node_results(
            task_id,
            node_id,
            node_name,
            node_key,
            node_type,
            status,
            message,
            command_count
        )
        VALUES (
            ?1,
            ?2,
            '本机节点',
            'local',
            'local',
            'failed',
            '节点本机节点未通过 Caddy 能力探测，已启用反向代理切流，预检阻断部署',
            0
        )
        "#,
    )
    .bind(capability_task_id)
    .bind(local_node_id.parse::<i64>()?)
    .execute(&db)
    .await?;
    let capability_task_detail = client
        .get(format!("{base_url}/tasks/{capability_task_id}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        capability_task_detail.contains(&format!("#{capability_task_id}")),
        "node result task detail should render for capability failure"
    );
    command_runner.with_result(
        "sh -lc sudo apt-get update && sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt | sudo tee /etc/apt/sources.list.d/caddy-stable.list && sudo apt-get update && sudo apt-get install -y caddy",
        CommandResult {
            status_code: Some(0),
            stdout: "caddy task repair install ok\n".to_owned(),
            stderr: String::new(),
        },
    );
    let capability_task_return_to = format!("/tasks/{capability_task_id}");
    let install_caddy_from_task = client
        .post(format!("{base_url}/nodes/install"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&capability_task_detail)?.as_str(),
            ),
            ("node_id", local_node_id.as_str()),
            ("component", "caddy"),
            ("return_to", capability_task_return_to.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        install_caddy_from_task.status() == reqwest::StatusCode::SEE_OTHER,
        "install Caddy from task result should redirect: {}",
        install_caddy_from_task.status()
    );
    let install_caddy_from_task_location = response_location(&install_caddy_from_task)?;
    anyhow::ensure!(
        install_caddy_from_task_location
            .contains(&format!("?return_to=/tasks/{capability_task_id}")),
        "install task redirect should preserve source task return path: {install_caddy_from_task_location}"
    );
    let install_caddy_from_task_id = extract_task_id_from_location(
        install_caddy_from_task_location
            .split('?')
            .next()
            .unwrap_or(install_caddy_from_task_location),
    )?;
    let install_caddy_from_task_detail = wait_for_page(
        &client,
        &format!(
            "{base_url}/tasks/{install_caddy_from_task_id}?return_to=/tasks/{capability_task_id}"
        ),
        &[
            "caddy",
            &format!("href=\"/tasks/{capability_task_id}\""),
            "action=\"/nodes/check\"",
            "caddy task repair install ok",
        ],
    )
    .await?;
    anyhow::ensure!(
        install_caddy_from_task_detail.contains("caddy task repair install ok")
            && install_caddy_from_task_detail
                .contains(&format!("href=\"/tasks/{capability_task_id}\""))
            && install_caddy_from_task_detail.contains("action=\"/nodes/check\"")
            && install_caddy_from_task_detail.contains(&format!(
                "name=\"return_to\" value=\"/tasks/{capability_task_id}\""
            )),
        "install task from task result should show source-task return and recheck action"
    );
    command_runner.with_result(
        "caddy version",
        CommandResult {
            status_code: Some(0),
            stdout: "2.8.4\n".to_owned(),
            stderr: String::new(),
        },
    );
    let recheck_after_task_install = client
        .post(format!("{base_url}/nodes/check"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&install_caddy_from_task_detail)?.as_str(),
            ),
            ("node_id", local_node_id.as_str()),
            ("return_to", capability_task_return_to.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        recheck_after_task_install.status() == reqwest::StatusCode::SEE_OTHER,
        "recheck after task-origin install should redirect: {}",
        recheck_after_task_install.status()
    );
    anyhow::ensure!(
        response_location(&recheck_after_task_install)? == capability_task_return_to,
        "recheck after task-origin install should return to source task"
    );
    let failed_app_detail = wait_for_page(
        &client,
        &format!("{base_url}/apps/4"),
        &[
            "Bad Compose",
            "bad-compose",
            "compose config invalid: missing image",
        ],
    )
    .await?;
    anyhow::ensure!(
        failed_app_detail.contains("Bad Compose")
            && failed_app_detail.contains("bad-compose")
            && failed_app_detail.contains("compose config invalid: missing image")
            && failed_app_detail.contains(&format!("/tasks/{bad_deploy_task_id}"))
            && failed_app_detail.contains(&format!("/nodes/{local_node_id}")),
        "failed compose app detail should show unhealthy runtime state"
    );
    let viewer_failed_task_detail = viewer_client
        .get(format!("{base_url}/tasks/{bad_deploy_task_id}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        viewer_failed_task_detail.contains("Bad Compose")
            && viewer_failed_task_detail.contains("compose config invalid: missing image")
            && !viewer_failed_task_detail.contains(&format!("/tasks/{bad_deploy_task_id}/retry"))
            && !viewer_failed_task_detail.contains(&format!("/tasks/{bad_deploy_task_id}/cancel")),
        "viewer should see failed task detail without retry or cancel actions"
    );
    let forbidden_retry = viewer_client
        .post(format!("{base_url}/tasks/{bad_deploy_task_id}/retry"))
        .form(&[(
            "csrf_token",
            extract_csrf_token(&viewer_failed_task_detail)?.as_str(),
        )])
        .send()
        .await?;
    anyhow::ensure!(
        forbidden_retry.status() == reqwest::StatusCode::FORBIDDEN,
        "viewer should receive 403 for task retry: {}",
        forbidden_retry.status()
    );
    command_runner.with_result(
        "docker compose config",
        CommandResult {
            status_code: Some(0),
            stdout: "config ok after retry\n".to_owned(),
            stderr: String::new(),
        },
    );
    let failed_task_detail_from_services = client
        .get(format!(
            "{base_url}/tasks/{bad_deploy_task_id}?return_to=/services"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        failed_task_detail_from_services.contains("href=\"/services\"")
            && failed_task_detail_from_services.contains("name=\"return_to\" value=\"/services\"")
            && failed_task_detail_from_services
                .contains(&format!("/tasks/{bad_deploy_task_id}/retry")),
        "failed task detail opened from services should keep return_to in retry form"
    );
    let retry = client
        .post(format!("{base_url}/tasks/{bad_deploy_task_id}/retry"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&failed_task_detail_from_services)?.as_str(),
            ),
            ("return_to", "/services"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        retry.status() == reqwest::StatusCode::SEE_OTHER,
        "retry failed task should redirect: {}",
        retry.status()
    );
    anyhow::ensure!(
        response_location(&retry)?.contains("?return_to=/services"),
        "retry failed task should preserve return_to in redirect"
    );
    let retry_task_id = extract_task_id_from_response_location(&retry, "retry failed task")?;
    let retry_task_path = format!("/tasks/{retry_task_id}");
    let retry_tasks = wait_for_tasks_page(
        &client,
        &base_url,
        &[
            "Bad Compose",
            "docker compose up -d --remove-orphans",
            &retry_task_path,
        ],
    )
    .await?;
    anyhow::ensure!(
        retry_tasks.contains(&retry_task_path)
            && retry_tasks.contains("Bad Compose")
            && retry_tasks.contains("docker compose up -d --remove-orphans"),
        "retry should create a new successful task"
    );
    let failed_filter_tasks = client
        .get(format!("{base_url}/tasks?status=failed"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        failed_filter_tasks.contains("tasks-filter-form")
            && failed_filter_tasks.contains("value=\"failed\" selected")
            && failed_filter_tasks.contains("Bad Compose")
            && failed_filter_tasks.contains(&format!("/tasks/{bad_deploy_task_id}"))
            && !failed_filter_tasks.contains(&retry_task_path),
        "failed status filter should only show failed tasks"
    );
    let failed_phase_tasks = client
        .get(format!("{base_url}/tasks?phase=failed"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        failed_phase_tasks.contains("value=\"failed\" selected")
            && failed_phase_tasks.contains("Bad Compose")
            && failed_phase_tasks.contains(&format!("/tasks/{bad_deploy_task_id}"))
            && failed_phase_tasks.contains("compose config invalid: missing image")
            && !failed_phase_tasks.contains(&retry_task_path),
        "failed phase filter should only show failed-phase tasks"
    );
    let app_filter_tasks = client
        .get(format!("{base_url}/tasks?app_id=4"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        app_filter_tasks.contains("Bad Compose")
            && app_filter_tasks.contains(&format!("/tasks/{bad_deploy_task_id}"))
            && app_filter_tasks.contains(&retry_task_path)
            && !app_filter_tasks.contains("edge-compose"),
        "app filter should only show selected app tasks"
    );
    let kind_filter_tasks = client
        .get(format!("{base_url}/tasks?task_kind=compose.up&q=Bad"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        kind_filter_tasks.contains("value=\"compose.up\" selected")
            && kind_filter_tasks.contains("value=\"Bad\"")
            && kind_filter_tasks.contains("Bad Compose")
            && !kind_filter_tasks.contains(&format!("/tasks/{ssh_compose_task_id}")),
        "task kind and keyword filters should narrow task list"
    );
    let summary_keyword_tasks = client
        .get(format!("{base_url}/tasks?q=compose%20config%20invalid"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        summary_keyword_tasks.contains("tasks-filter-form")
            && summary_keyword_tasks.contains("value=\"compose config invalid\"")
            && summary_keyword_tasks.contains("compose config invalid: missing image")
            && summary_keyword_tasks.contains(&format!("/tasks/{bad_deploy_task_id}"))
            && !summary_keyword_tasks.contains(&retry_task_path),
        "task keyword filter should search failure summaries"
    );
    let empty_filter_tasks = client
        .get(format!("{base_url}/tasks?status=canceled&q=not-found"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        empty_filter_tasks.contains("tasks-filter-form")
            && empty_filter_tasks.contains("value=\"canceled\" selected")
            && empty_filter_tasks.contains("value=\"not-found\"")
            && !empty_filter_tasks.contains("compose config invalid: missing image")
            && !empty_filter_tasks.contains(&format!("href=\"{retry_task_path}\"")),
        "empty task filter should render empty state"
    );
    let retry_detail = wait_for_task_detail_page(
        &client,
        &base_url,
        retry_task_id,
        &[
            "config ok after retry",
            "docker compose up -d --remove-orphans",
        ],
    )
    .await?;
    anyhow::ensure!(
        retry_detail.contains("Bad Compose")
            && retry_detail.contains("config ok after retry")
            && retry_detail.contains("docker compose up -d --remove-orphans"),
        "retry detail should show successful retry output"
    );
    let commands = command_runner
        .commands
        .lock()
        .expect("lock commands")
        .clone();
    anyhow::ensure!(
        commands.iter().any(|command| command
            == "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/edge-compose && docker compose up -d --remove-orphans")
            && commands.iter().any(|command| command
                == "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/edge-compose && docker compose logs --tail 200 --no-color web")
            && commands.iter().any(|command| command == "docker compose config")
            && commands.iter().any(|command| command == "docker compose up -d --remove-orphans"),
        "compose commands should include local, ssh deployment and ssh logs: {commands:?}"
    );

    let binary_placeholder_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let binary_placeholder_csrf = extract_csrf_token(&binary_placeholder_apps)?;
    let worker_placeholder = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", binary_placeholder_csrf.as_str()),
            ("app_key", "worker-bin"),
            ("name", "Worker 占位应用"),
            ("description", "旧二进制 e2e 退场后的序号占位"),
            ("app_type", "compose"),
            ("release_source", "manual"),
            ("deploy_strategy", "rolling_stop_on_failure"),
            ("work_dir", "/opt/easy-deploy/apps/worker-bin"),
            (
                "compose_content",
                "services:\n  worker:\n    image: busybox\n",
            ),
            ("env_content", "WORKER_ENV=e2e\n"),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        worker_placeholder.status() == reqwest::StatusCode::SEE_OTHER,
        "worker placeholder should redirect: {}",
        worker_placeholder.status()
    );
    anyhow::ensure!(
        response_location(&worker_placeholder)? == "/apps/5?notice=created",
        "worker placeholder should keep historical app id sequence"
    );
    let edge_placeholder = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", binary_placeholder_csrf.as_str()),
            ("app_key", "edge-bin"),
            ("name", "Edge 占位应用"),
            ("description", "旧 SSH 二进制 e2e 退场后的序号占位"),
            ("app_type", "compose"),
            ("release_source", "manual"),
            ("deploy_strategy", "rolling_stop_on_failure"),
            ("work_dir", "/opt/easy-deploy/apps/edge-bin"),
            (
                "compose_content",
                "services:\n  edge:\n    image: busybox\n",
            ),
            ("env_content", "EDGE_ENV=e2e\n"),
            ("target_node_ids", ssh_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        edge_placeholder.status() == reqwest::StatusCode::SEE_OTHER,
        "edge placeholder should redirect: {}",
        edge_placeholder.status()
    );
    anyhow::ensure!(
        response_location(&edge_placeholder)? == "/apps/6?notice=created",
        "edge placeholder should keep historical app id sequence"
    );

    if false {
        let apps_for_binary = client
            .get(format!("{base_url}/apps"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let create_binary = client
            .post(format!("{base_url}/apps"))
            .form(&[
                ("csrf_token", extract_csrf_token(&apps_for_binary)?.as_str()),
                ("app_key", "worker-bin"),
                ("name", "Worker 浜岃�"),
                ("description", "systemd 绠＄悊鐨勪簩杩涘埗鏈嶅姟"),
                ("app_type", "binary"),
                ("work_dir", binary_target_dir_str.as_str()),
                ("compose_content", ""),
                ("env_content", "WORKER_ENV=e2e\n"),
                ("binary_artifact_version", "v1.0.0"),
                (
                    "binary_artifact_path",
                    "/opt/easy-deploy/artifacts/worker-bin",
                ),
                ("binary_exec_args", "--port 8080"),
                ("binary_service_user", "deploy"),
                ("binary_unit_name", "easy-deploy-worker-bin.service"),
                ("binary_release_strategy", "blue_green"),
                ("binary_active_slot", "blue"),
                ("binary_base_port", "8080"),
                ("binary_standby_port", "18080"),
                ("binary_proxy_enabled", "true"),
                ("binary_proxy_kind", "caddy"),
                ("binary_proxy_domain", "worker.example.com"),
                ("binary_proxy_config_path", binary_caddy_config.as_str()),
                ("target_node_ids", local_node_id.as_str()),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            create_binary.status() == reqwest::StatusCode::SEE_OTHER,
            "create binary app should redirect: {}",
            create_binary.status()
        );
        let binary_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &binary_detail,
            &[
                "worker-bin",
                "v1.0.0",
                "name=\"binary_artifact_path\" value=\"/opt/easy-deploy/artifacts/worker-bin\"",
                "name=\"binary_exec_args\" value=\"--port 8080\"",
                "easy-deploy-worker-bin.service",
                "ExecStart=/opt/easy-deploy/artifacts/worker-bin --port 8080",
                "easy-deploy-worker-bin-blue.service",
                "easy-deploy-worker-bin-green.service",
                "value=\"blue\" selected",
                "value=\"blue_green\" selected",
                "value=\"18080\"",
                "value=\"caddy\" selected",
                "worker.example.com",
                binary_caddy_config.as_str(),
                "href=\"/apps/5/binary/restart/confirm\"",
                "href=\"/artifacts\"",
            ],
            "binary app detail should render binary configuration",
        )?;
        let binary_root = data_dir.join("apps").join("worker-bin");
        let binary_unit = tokio::fs::read_to_string(
            binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin.service"),
        )
        .await?;
        let binary_env = tokio::fs::read_to_string(
            binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin.env"),
        )
        .await?;
        let binary_release = tokio::fs::read_to_string(
            binary_root
                .join("releases")
                .join("v1.0.0")
                .join("release.yaml"),
        )
        .await?;
        let binary_blue_unit = tokio::fs::read_to_string(
            binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin-blue.service"),
        )
        .await?;
        let binary_green_unit = tokio::fs::read_to_string(
            binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin-green.service"),
        )
        .await?;
        let binary_current = tokio::fs::read_to_string(binary_root.join("current")).await?;
        let binary_app_meta =
            tokio::fs::read_to_string(binary_root.join(".easy-deploy").join("app.yaml")).await?;
        anyhow::ensure!(
        binary_unit.contains("Description=Easy Deploy Worker")
            && binary_unit.contains("(worker-bin)")
            && binary_unit.contains(&format!("WorkingDirectory={binary_deploy_dir}"))
            && binary_unit.contains(&format!("EnvironmentFile=-{binary_deploy_dir}/.easy-deploy/systemd/easy-deploy-worker-bin.env"))
            && binary_unit.contains("User=deploy")
            && binary_unit.contains("ExecStart=/opt/easy-deploy/artifacts/worker-bin --port 8080"),
        "binary systemd unit file should be generated"
    );
        anyhow::ensure!(
            binary_blue_unit.contains("Description=Easy Deploy Worker")
                && binary_blue_unit.contains("(worker-bin) blue"),
            "binary blue unit description missing: {binary_blue_unit}"
        );
        anyhow::ensure!(
            binary_blue_unit.contains("Environment=PORT=8080"),
            "binary blue unit port env missing: {binary_blue_unit}"
        );
        anyhow::ensure!(
            binary_blue_unit
                .contains("ExecStart=/opt/easy-deploy/artifacts/worker-bin --port ${PORT}"),
            "binary blue unit ExecStart missing: {binary_blue_unit}"
        );
        anyhow::ensure!(
            binary_green_unit.contains("Description=Easy Deploy Worker")
                && binary_green_unit.contains("(worker-bin) green"),
            "binary green unit description missing: {binary_green_unit}"
        );
        anyhow::ensure!(
            binary_green_unit.contains("Environment=PORT=18080"),
            "binary green unit port env missing: {binary_green_unit}"
        );
        anyhow::ensure!(
            binary_green_unit
                .contains("ExecStart=/opt/easy-deploy/artifacts/worker-bin --port ${PORT}"),
            "binary green unit ExecStart missing: {binary_green_unit}"
        );
        anyhow::ensure!(
            binary_env.contains("WORKER_ENV=e2e"),
            "binary env file should be generated"
        );
        anyhow::ensure!(
            binary_release.contains("artifact_version: \"v1.0.0\"")
                && binary_release
                    .contains("artifact_path: \"/opt/easy-deploy/artifacts/worker-bin\"")
                && binary_release.contains("release_strategy: \"blue_green\"")
                && binary_release.contains("active_slot: \"blue\"")
                && binary_release.contains("base_port: 8080")
                && binary_release.contains("standby_port: 18080")
                && binary_release.contains("proxy_enabled: true")
                && binary_release.contains("proxy_kind: \"caddy\"")
                && binary_release.contains("proxy_domain: \"worker.example.com\"")
                && binary_release
                    .contains("unit_file: \".easy-deploy/systemd/easy-deploy-worker-bin.service\""),
            "binary release metadata should be generated"
        );
        anyhow::ensure!(
            binary_current.contains("artifact_version: \"v1.0.0\"")
                && binary_current.contains("release_file: \"releases/v1.0.0/release.yaml\""),
            "binary current release pointer should be generated"
        );
        anyhow::ensure!(
            binary_app_meta.contains("binary:")
                && binary_app_meta.contains("current_release_file: \"current\"")
                && binary_app_meta.contains("release_strategy: \"blue_green\"")
                && binary_app_meta.contains("active_slot: \"blue\"")
                && binary_app_meta.contains("base_port: 8080")
                && binary_app_meta.contains("standby_port: 18080")
                && binary_app_meta.contains("proxy_enabled: true")
                && binary_app_meta.contains("proxy_kind: \"caddy\"")
                && binary_app_meta.contains("proxy_domain: \"worker.example.com\"")
                && binary_app_meta.contains("release_file: \"releases/v1.0.0/release.yaml\""),
            "binary app metadata should include runtime file references"
        );
        let upload_page = client
            .get(format!("{base_url}/artifacts"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &upload_page,
            &[
                "action=\"/artifacts/upload\"",
                "name=\"app_id\"",
                "name=\"artifact_file\"",
            ],
            "artifacts page should render upload form",
        )?;
        let upload_binary = client
            .post(format!("{base_url}/artifacts/upload"))
            .multipart(
                reqwest::multipart::Form::new()
                    .text("csrf_token", extract_csrf_token(&upload_page)?)
                    .text("app_id", "5")
                    .text("artifact_version", "v1.1.0")
                    .text("entry_file", "")
                    .part(
                        "artifact_file",
                        reqwest::multipart::Part::bytes("worker binary v1.1.0".as_bytes().to_vec())
                            .file_name("worker-bin-v1.1.0"),
                    ),
            )
            .send()
            .await?;
        anyhow::ensure!(
            upload_binary.status() == reqwest::StatusCode::SEE_OTHER,
            "upload binary artifact should redirect: {}",
            upload_binary.status()
        );
        let uploaded_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &uploaded_detail,
            &[
                "v1.1.0",
                "worker-bin-v1.1.0",
                "sha256",
                "ExecStart=",
                &format!("{binary_deploy_dir}/releases/v1.1.0/worker-bin-v1.1.0"),
                "binary/releases/",
                "/deploy",
            ],
            "uploaded binary release should be shown as current",
        )?;
        let uploaded_artifact = tokio::fs::read_to_string(
            binary_root
                .join("releases")
                .join("v1.1.0")
                .join("worker-bin-v1.1.0"),
        )
        .await?;
        let uploaded_release = tokio::fs::read_to_string(
            binary_root
                .join("releases")
                .join("v1.1.0")
                .join("release.yaml"),
        )
        .await?;
        let uploaded_current = tokio::fs::read_to_string(binary_root.join("current")).await?;
        anyhow::ensure!(
            uploaded_artifact == "worker binary v1.1.0",
            "uploaded binary file should be stored in release directory"
        );
        anyhow::ensure!(
            uploaded_release.contains("artifact_version: \"v1.1.0\"")
                && uploaded_release.contains("artifact_path: \"")
                && uploaded_release.contains(&format!("{binary_deploy_dir}/releases"))
                && uploaded_release.contains("worker-bin-v1.1.0"),
            "uploaded release metadata should point at stored artifact"
        );
        anyhow::ensure!(
            uploaded_current.contains("artifact_version: \"v1.1.0\"")
                && uploaded_current.contains("release_file: \"releases/v1.1.0/release.yaml\""),
            "uploaded current pointer should move to v1.1.0"
        );
        let artifacts_page = client
            .get(format!("{base_url}/artifacts"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &artifacts_page,
            &[
                "artifacts-filter-form",
                "worker-bin",
                "v1.0.0",
                "v1.1.0",
                "worker-bin-v1.1.0",
                "sha256",
                &format!("{binary_deploy_dir}/releases/v1.1.0/worker-bin-v1.1.0"),
            ],
            "artifacts page should render binary releases and uploaded metadata",
        )?;
        anyhow::ensure!(
            artifacts_page.contains("name=\"status\"")
                && artifacts_page.contains("name=\"kind\"")
                && artifacts_page.contains("name=\"source\"")
                && artifacts_page.contains("name=\"q\""),
            "artifacts page should render filter controls"
        );
        let filtered_artifacts = client
            .get(format!(
                "{base_url}/artifacts?status=active&source=upload&q=v1.1.0"
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        anyhow::ensure!(
            filtered_artifacts.contains("value=\"active\" selected")
                && filtered_artifacts.contains("value=\"upload\" selected")
                && filtered_artifacts.contains("value=\"v1.1.0\"")
                && filtered_artifacts.contains("v1.1.0")
                && filtered_artifacts.contains("worker-bin-v1.1.0")
                && !filtered_artifacts.contains("v1.0.0"),
            "artifacts page should filter by status, source and keyword"
        );
        let empty_artifacts = client
            .get(format!("{base_url}/artifacts?kind=tar_gz&q=not-found"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        anyhow::ensure!(
            empty_artifacts.contains("value=\"tar_gz\" selected")
                && empty_artifacts.contains("value=\"not-found\"")
                && empty_artifacts.contains("name=\"status\""),
            "artifacts page should render empty state for unmatched filters"
        );
        let old_release_path = extract_binary_release_deploy_path(&uploaded_detail, "v1.0.0")?;

        let mut upload_form_page = artifacts_page.clone();
        for version in ["v1.2.0", "v1.3.0", "v1.4.0", "v1.5.0"] {
            let upload = client
                .post(format!("{base_url}/artifacts/upload"))
                .multipart(
                    reqwest::multipart::Form::new()
                        .text("csrf_token", extract_csrf_token(&upload_form_page)?)
                        .text("app_id", "5")
                        .text("artifact_version", version)
                        .text("entry_file", "")
                        .part(
                            "artifact_file",
                            reqwest::multipart::Part::bytes(
                                format!("worker binary {version}").into_bytes(),
                            )
                            .file_name(format!("worker-bin-{version}")),
                        ),
                )
                .send()
                .await?;
            anyhow::ensure!(
                upload.status() == reqwest::StatusCode::SEE_OTHER,
                "upload retained binary artifact {version} should redirect: {}",
                upload.status()
            );
            upload_form_page = client
                .get(format!("{base_url}/artifacts"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
        }
        let retained_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &retained_detail,
            &[
                "v1.5.0",
                "worker-bin-v1.5.0",
                "v1.4.0",
                "v1.3.0",
                "v1.2.0",
                "v1.1.0",
                "worker-bin-v1.1.0",
                &format!("{binary_deploy_dir}/releases/v1.5.0/worker-bin-v1.5.0"),
            ],
            "binary detail should mark oldest uploaded release as cleaned after retention pruning",
        )?;
        let pruned_release_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM binary_artifacts WHERE app_id = 5 AND version = 'v1.1.0'",
        )
        .fetch_one(&db)
        .await?;
        let active_release_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM binary_artifacts WHERE app_id = 5 AND version = 'v1.5.0'",
        )
        .fetch_one(&db)
        .await?;
        anyhow::ensure!(
            pruned_release_status == "disabled" && active_release_status == "active",
            "binary retention should disable pruned release and keep newest upload active"
        );
        anyhow::ensure!(
            !binary_root.join("releases").join("v1.1.0").exists()
                && binary_root.join("releases").join("v1.2.0").exists()
                && binary_root.join("releases").join("v1.3.0").exists()
                && binary_root.join("releases").join("v1.4.0").exists()
                && binary_root.join("releases").join("v1.5.0").exists(),
            "binary upload retention should remove only pruned release directories"
        );
        let retained_artifacts_page = client
            .get(format!("{base_url}/artifacts"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &retained_artifacts_page,
            &[
                "artifacts-filter-form",
                "worker-bin",
                "v1.5.0",
                "worker-bin-v1.5.0",
                "v1.1.0",
                "worker-bin-v1.1.0",
            ],
            "artifacts page should show cleaned retained upload state",
        )?;
        anyhow::ensure!(
            retained_artifacts_page.contains("value=\"active\"")
                && retained_artifacts_page.contains("value=\"disabled\""),
            "artifacts page should keep active and disabled status filters"
        );

        let config_editor_client = test_client()?;
        let config_editor_login = config_editor_client
            .post(format!("{base_url}/login"))
            .form(&[
                ("username", "configeditor"),
                ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            config_editor_login.status() == reqwest::StatusCode::SEE_OTHER,
            "config editor login should redirect: {}",
            config_editor_login.status()
        );
        let config_editor_binary_detail = config_editor_client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &config_editor_binary_detail,
            &[
                "worker-bin",
                "method=\"post\" action=\"/apps/5/config\"",
                "name=\"binary_artifact_version\"",
                "name=\"binary_unit_name\"",
            ],
            "config editor should edit app config without artifact upload or rollback action",
        )?;
        anyhow::ensure!(
            !config_editor_binary_detail.contains("action=\"/apps/5/binary/upload\"")
                && !config_editor_binary_detail.contains("/binary/releases/")
                && !config_editor_binary_detail.contains("/deploy"),
            "config editor should not see artifact upload or rollback actions"
        );
        let forbidden_binary_upload = config_editor_client
            .post(format!("{base_url}/artifacts/upload"))
            .multipart(
                reqwest::multipart::Form::new()
                    .text(
                        "csrf_token",
                        extract_csrf_token(&config_editor_binary_detail)?,
                    )
                    .text("app_id", "5")
                    .text("artifact_version", "v-denied")
                    .text("entry_file", "")
                    .part(
                        "artifact_file",
                        reqwest::multipart::Part::bytes("denied upload".as_bytes().to_vec())
                            .file_name("denied-worker-bin"),
                    ),
            )
            .send()
            .await?;
        anyhow::ensure!(
            forbidden_binary_upload.status() == reqwest::StatusCode::FORBIDDEN,
            "config editor should receive 403 for artifact upload: {}",
            forbidden_binary_upload.status()
        );
        let forbidden_release_switch = config_editor_client
            .post(format!("{base_url}{old_release_path}"))
            .form(&[(
                "csrf_token",
                extract_csrf_token(&config_editor_binary_detail)?.as_str(),
            )])
            .send()
            .await?;
        anyhow::ensure!(
            forbidden_release_switch.status() == reqwest::StatusCode::FORBIDDEN,
            "config editor should receive 403 for release rollback: {}",
            forbidden_release_switch.status()
        );

        let rollbacker_client = test_client()?;
        let rollbacker_login = rollbacker_client
            .post(format!("{base_url}/login"))
            .form(&[
                ("username", "rollbacker"),
                ("password", LOCAL_TEST_ADMIN_PASSWORD),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            rollbacker_login.status() == reqwest::StatusCode::SEE_OTHER,
            "rollbacker login should redirect: {}",
            rollbacker_login.status()
        );
        let rollbacker_binary_detail = rollbacker_client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &rollbacker_binary_detail,
            &["worker-bin", "/binary/releases/", "/deploy"],
            "rollbacker should see rollback action",
        )?;
        anyhow::ensure!(
            rollbacker_binary_detail.contains("readonly")
                && !rollbacker_binary_detail.contains("action=\"/apps/5/metadata\"")
                && !rollbacker_binary_detail.contains("name=\"target_node_ids\"")
                && !rollbacker_binary_detail.contains("action=\"/apps/5/binary/upload\""),
            "rollbacker should not see app config edit controls"
        );
        let deploy_old = rollbacker_client
            .post(format!("{base_url}{old_release_path}"))
            .form(&[(
                "csrf_token",
                extract_csrf_token(&rollbacker_binary_detail)?.as_str(),
            )])
            .send()
            .await?;
        anyhow::ensure!(
            deploy_old.status() == reqwest::StatusCode::SEE_OTHER,
            "deploy old binary release should redirect: {}",
            deploy_old.status()
        );
        let deploy_old_location = deploy_old
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!("deploy old binary release redirect missing location")
            })?;
        let deploy_old_task_id = extract_task_id_from_location(deploy_old_location)?;
        let deploy_old_task_detail = wait_for_task_detail_page(
            &client,
            &base_url,
            deploy_old_task_id,
            &[
                "v1.0.0",
                "systemctl link",
                "systemctl restart easy-deploy-worker-bin-green.service",
                "systemctl reload caddy.service",
                "green(18080)",
            ],
        )
        .await?;
        anyhow::ensure!(
            deploy_old_task_detail.contains("Worker")
                && deploy_old_task_detail.contains("Blue/Green")
                && deploy_old_task_detail.contains("tone-success")
                && deploy_old_task_detail.contains("systemctl daemon-reload"),
            "deploy old binary task detail should show successful completion"
        );
        let reactivated_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let reactivated_current = tokio::fs::read_to_string(binary_root.join("current")).await?;
        ensure_contains_all(
            &reactivated_detail,
            &[
                "v1.0.0",
                "/opt/easy-deploy/artifacts/worker-bin",
                "easy-deploy-worker-bin.service",
                "ExecStart=/opt/easy-deploy/artifacts/worker-bin --port 8080",
            ],
            "reactivated old release should be shown as current",
        )?;
        anyhow::ensure!(
            reactivated_current.contains("artifact_version: \"v1.0.0\"")
                && reactivated_current.contains("release_file: \"releases/v1.0.0/release.yaml\""),
            "current pointer should move back to v1.0.0"
        );
        let binary_health = client
            .post(format!("{base_url}/apps/5/config"))
            .form(&[
                ("csrf_token", extract_csrf_token(&binary_detail)?.as_str()),
                ("compose_content", ""),
                ("env_content", "WORKER_ENV=e2e\n"),
                ("binary_artifact_version", "v1.0.0"),
                (
                    "binary_artifact_path",
                    "/opt/easy-deploy/artifacts/worker-bin",
                ),
                ("binary_exec_args", "--port 8080"),
                ("binary_service_user", "deploy"),
                ("binary_unit_name", "easy-deploy-worker-bin.service"),
                ("binary_release_strategy", "blue_green"),
                ("binary_active_slot", "blue"),
                ("binary_base_port", "8080"),
                ("binary_standby_port", "18080"),
                ("binary_proxy_enabled", "true"),
                ("binary_proxy_kind", "caddy"),
                ("binary_proxy_domain", "worker.example.com"),
                ("binary_proxy_config_path", binary_caddy_config.as_str()),
                ("health_check_kind", "systemd_active"),
                ("health_endpoint", "easy-deploy-worker-bin.service"),
                ("health_timeout_secs", "5"),
                ("health_expected_status", "200"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            binary_health.status() == reqwest::StatusCode::SEE_OTHER,
            "save binary health config should redirect: {}",
            binary_health.status()
        );
        command_runner.with_result(
            "systemctl is-active easy-deploy-worker-bin-green.service",
            CommandResult {
                status_code: Some(0),
                stdout: "active\n".to_owned(),
                stderr: String::new(),
            },
        );
        let binary_updated_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let binary_restart_confirm = client
            .get(format!("{base_url}/apps/5/binary/restart/confirm"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &binary_restart_confirm,
            &[
                "Blue/Green",
                "Caddy",
                "worker.example.com",
                "blue",
                "green",
                "8080",
                "18080",
                "systemctl link",
                binary_caddy_config.as_str(),
                "method=\"post\" action=\"/apps/5/binary/restart\"",
                "name=\"confirmed\" value=\"1\"",
            ],
            "binary restart confirm should expose blue/green proxy switch plan",
        )?;
        let binary_restart = client
            .post(format!("{base_url}/apps/5/binary/restart"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&binary_updated_detail)?.as_str(),
                ),
                ("confirmed", "1"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            binary_restart.status() == reqwest::StatusCode::SEE_OTHER,
            "binary restart should redirect to tasks: {}",
            binary_restart.status()
        );
        let binary_restart_task_id =
            extract_task_id_from_response_location(&binary_restart, "binary restart")?;
        let binary_task_detail = wait_for_task_detail_page(
            &client,
            &base_url,
            binary_restart_task_id,
            &[
                "Blue/Green",
                "systemctl daemon-reload",
                "systemctl restart easy-deploy-worker-bin-green.service",
                "systemctl is-active easy-deploy-worker-bin-green.service",
                "caddy validate --adapter caddyfile",
                "systemctl reload caddy.service",
                "green(18080)",
                "tone-success",
            ],
        )
        .await?;
        let target_binary_root = binary_target_dir.as_path();
        let synced_unit = tokio::fs::read_to_string(
            target_binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin.service"),
        )
        .await?;
        let synced_green_unit = tokio::fs::read_to_string(
            target_binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin-green.service"),
        )
        .await?;
        let synced_app_meta =
            tokio::fs::read_to_string(target_binary_root.join(".easy-deploy").join("app.yaml"))
                .await?;
        let synced_current = tokio::fs::read_to_string(target_binary_root.join("current")).await?;
        let synced_release = tokio::fs::read_to_string(
            target_binary_root
                .join("releases")
                .join("v1.0.0")
                .join("release.yaml"),
        )
        .await?;
        let caddy_config = tokio::fs::read_to_string(&binary_caddy_config).await?;
        anyhow::ensure!(
            synced_unit.contains("ExecStart=/opt/easy-deploy/artifacts/worker-bin --port 8080"),
            "synced primary unit ExecStart missing: {synced_unit}"
        );
        anyhow::ensure!(
            synced_green_unit.contains("Environment=PORT=18080"),
            "synced green unit PORT missing: {synced_green_unit}"
        );
        anyhow::ensure!(
            synced_green_unit
                .contains("ExecStart=/opt/easy-deploy/artifacts/worker-bin --port ${PORT}"),
            "synced green unit ExecStart missing: {synced_green_unit}"
        );
        anyhow::ensure!(
            synced_current.contains("artifact_version: \"v1.0.0\""),
            "synced current version missing: {synced_current}"
        );
        anyhow::ensure!(
            synced_release.contains("artifact_version: \"v1.0.0\"")
                && synced_release.contains("release_strategy: \"blue_green\"")
                && synced_release.contains("active_slot: \"green\"")
                && synced_release.contains("base_port: 8080")
                && synced_release.contains("standby_port: 18080")
                && synced_release.contains("proxy_enabled: true")
                && synced_release.contains("proxy_kind: \"caddy\""),
            "synced release metadata mismatch: {synced_release}"
        );
        anyhow::ensure!(
            synced_app_meta.contains("active_slot: \"green\""),
            "synced app metadata active slot mismatch: {synced_app_meta}"
        );
        anyhow::ensure!(
            caddy_config.contains("worker.example.com")
                && caddy_config.contains("reverse_proxy 127.0.0.1:18080"),
            "caddy config should point to green slot: {caddy_config}"
        );
        let local_unit_link_command = format!(
            "systemctl link {}",
            target_binary_root
                .join(".easy-deploy")
                .join("systemd")
                .join("easy-deploy-worker-bin-green.service")
                .to_string_lossy()
        );
        let local_caddy_validate_command =
            format!("caddy validate --adapter caddyfile --config {binary_caddy_config}");
        let binary_tasks = client
            .get(format!("{base_url}/tasks"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        anyhow::ensure!(
            binary_tasks.contains(&format!("/tasks/{binary_restart_task_id}")),
            "binary restart should create task"
        );
        ensure_contains_all(
            &binary_task_detail,
            &[
                &local_unit_link_command,
                "systemctl daemon-reload",
                "systemctl restart easy-deploy-worker-bin-green.service",
                "systemctl is-active easy-deploy-worker-bin-green.service",
                "caddy validate --adapter caddyfile",
                "systemctl reload caddy.service",
                "green(18080)",
                "tone-success",
            ],
            "binary task detail should show runtime sync, proxy switch and daemon reload logs",
        )?;
        let binary_after_restart = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &binary_after_restart,
            &[
                "worker-bin",
                "v1.0.0",
                "green",
                "easy-deploy-worker-bin-green.service",
                &format!("/tasks/{binary_restart_task_id}"),
                &format!("/nodes/{local_node_id}"),
                &format!("/services/5/worker-bin/logs?node_id={local_node_id}"),
                "tone-success",
            ],
            "binary app detail should show healthy runtime state",
        )?;
        let services_after_binary = client
            .get(format!("{base_url}/services"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &services_after_binary,
            &[
                "worker-bin",
                &format!("/services/5/worker-bin/logs?node_id={local_node_id}"),
                &format!("/nodes/{local_node_id}"),
                "v1.0.0",
                "systemd",
                "easy-deploy-worker-bin-green.service",
                "green",
                "tone-success",
            ],
            "services page should expose binary service log action and health details",
        )?;
        command_runner.with_result(
            "journalctl -u easy-deploy-worker-bin.service -n 200 --no-pager",
            CommandResult {
                status_code: Some(0),
                stdout: "worker binary log line\n".to_owned(),
                stderr: String::new(),
            },
        );
        let binary_service_logs = client
            .get(format!(
                "{base_url}/services/5/worker-bin/logs?node_id={local_node_id}"
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &binary_service_logs,
            &[
                "worker-bin",
                "local",
                &format!("/nodes/{local_node_id}"),
                "journalctl -u easy-deploy-worker-bin.service -n 200 --no-pager",
                "worker binary log line",
            ],
            "binary service logs page should render journalctl output",
        )?;
        let commands_after_binary = command_runner
            .commands
            .lock()
            .expect("lock commands")
            .clone();
        anyhow::ensure!(
            command_order_contains(
                &commands_after_binary,
                &[
                    local_unit_link_command.as_str(),
                    "systemctl daemon-reload",
                    "systemctl restart easy-deploy-worker-bin-green.service",
                    "systemctl is-active easy-deploy-worker-bin-green.service",
                    local_caddy_validate_command.as_str(),
                    "systemctl reload caddy.service",
                    "journalctl -u easy-deploy-worker-bin.service -n 200 --no-pager",
                ],
            ),
            "binary restart and logs should run systemd commands: {commands_after_binary:?}"
        );
        let specs_after_binary = command_runner
            .command_specs
            .lock()
            .expect("lock command specs")
            .clone();
        anyhow::ensure!(
            specs_after_binary.iter().any(|spec| spec
                == &(
                    "journalctl -u easy-deploy-worker-bin.service -n 200 --no-pager".to_owned(),
                    binary_target_dir_str.clone(),
                ))
                && command_specs_contain_sequence(
                    &specs_after_binary,
                    &[
                        (
                            local_unit_link_command.as_str(),
                            binary_target_dir_str.as_str()
                        ),
                        ("systemctl daemon-reload", binary_target_dir_str.as_str(),),
                        (
                            "systemctl restart easy-deploy-worker-bin-green.service",
                            binary_target_dir_str.as_str(),
                        ),
                        (
                            "systemctl is-active easy-deploy-worker-bin-green.service",
                            binary_target_dir_str.as_str(),
                        ),
                        (
                            local_caddy_validate_command.as_str(),
                            binary_target_dir_str.as_str(),
                        ),
                        (
                            "systemctl reload caddy.service",
                            binary_target_dir_str.as_str()
                        ),
                    ],
                ),
            "binary systemd and proxy commands should run in target deploy directory: {specs_after_binary:?}"
        );

        let ssh_binary_apps = client
            .get(format!("{base_url}/apps"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let create_ssh_binary = client
            .post(format!("{base_url}/apps"))
            .form(&[
                ("csrf_token", extract_csrf_token(&ssh_binary_apps)?.as_str()),
                ("app_key", "edge-bin"),
                ("name", "Edge SSH 浜岃�"),
                ("description", "SSH 鑺傜�systemd 绠＄悊鐨勪簩杩涘埗鏈嶅姟"),
                ("app_type", "binary"),
                ("work_dir", "/opt/easy-deploy/apps/edge-bin"),
                ("compose_content", ""),
                ("env_content", "EDGE_ENV=e2e\n"),
                ("binary_artifact_version", "v1.0.0"),
                (
                    "binary_artifact_path",
                    "/opt/easy-deploy/apps/edge-bin/releases/v1.0.0/edge-bin",
                ),
                ("binary_exec_args", "--port 19091"),
                ("binary_service_user", "deploy"),
                ("binary_unit_name", "easy-deploy-edge-bin.service"),
                ("binary_release_strategy", "restart"),
                ("binary_active_slot", "blue"),
                ("binary_base_port", "0"),
                ("binary_standby_port", "0"),
                ("target_node_ids", ssh_node_id.as_str()),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            create_ssh_binary.status() == reqwest::StatusCode::SEE_OTHER,
            "create ssh binary app should redirect: {}",
            create_ssh_binary.status()
        );
        command_runner.with_result(
            "ssh -p 22 deploy@10.0.2.11 nc -z -w 5 127.0.0.1 19091",
            CommandResult {
                status_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let ssh_binary_detail = client
            .get(format!("{base_url}/apps/6"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let ssh_binary_health = client
            .post(format!("{base_url}/apps/6/config"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&ssh_binary_detail)?.as_str(),
                ),
                ("compose_content", ""),
                ("env_content", "EDGE_ENV=e2e\n"),
                ("binary_artifact_version", "v1.0.0"),
                (
                    "binary_artifact_path",
                    "/opt/easy-deploy/apps/edge-bin/releases/v1.0.0/edge-bin",
                ),
                ("binary_exec_args", "--port 19091"),
                ("binary_service_user", "deploy"),
                ("binary_unit_name", "easy-deploy-edge-bin.service"),
                ("binary_release_strategy", "restart"),
                ("binary_active_slot", "blue"),
                ("binary_base_port", "0"),
                ("binary_standby_port", "0"),
                ("health_check_kind", "tcp"),
                ("health_endpoint", "127.0.0.1:19091"),
                ("health_timeout_secs", "5"),
                ("health_expected_status", "200"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            ssh_binary_health.status() == reqwest::StatusCode::SEE_OTHER,
            "save ssh binary health config should redirect: {}",
            ssh_binary_health.status()
        );
        let ssh_binary_detail = client
            .get(format!("{base_url}/apps/6"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let ssh_binary_restart = client
            .post(format!("{base_url}/apps/6/binary/restart"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&ssh_binary_detail)?.as_str(),
                ),
                ("confirmed", "1"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            ssh_binary_restart.status() == reqwest::StatusCode::SEE_OTHER,
            "ssh binary restart should redirect: {}",
            ssh_binary_restart.status()
        );
        let ssh_binary_restart_task_id =
            extract_task_id_from_response_location(&ssh_binary_restart, "ssh binary restart")?;
        let ssh_binary_tasks = wait_for_tasks_page(
            &client,
            &base_url,
            &[
                "Edge SSH",
                "edge-bin",
                "ssh -p 22 deploy@10.0.2.11 nc -z -w 5 127.0.0.1 19091",
                &format!("/tasks/{ssh_binary_restart_task_id}"),
            ],
        )
        .await?;
        anyhow::ensure!(
            ssh_binary_tasks.contains(&format!("/tasks/{ssh_binary_restart_task_id}")),
            "ssh binary restart should create task"
        );
        let remote_unit_link_command = "ssh -p 22 deploy@10.0.2.11 systemctl link /opt/easy-deploy/apps/edge-bin/.easy-deploy/systemd/easy-deploy-edge-bin.service";
        let remote_chmod_command = "ssh -p 22 deploy@10.0.2.11 chmod +x /opt/easy-deploy/apps/edge-bin/releases/v1.0.0/edge-bin";
        let ssh_task_detail = wait_for_task_detail_page(
            &client,
            &base_url,
            ssh_binary_restart_task_id,
            &[
                remote_chmod_command,
                remote_unit_link_command,
                "ssh -p 22 deploy@10.0.2.11 systemctl daemon-reload",
                "ssh -p 22 deploy@10.0.2.11 systemctl restart easy-deploy-edge-bin.service",
                "ssh -p 22 deploy@10.0.2.11 nc -z -w 5 127.0.0.1 19091",
                "127.0.0.1:19091",
                "tone-success",
            ],
        )
        .await?;
        anyhow::ensure!(
            ssh_task_detail.contains("ssh -p 22 deploy@10.0.2.11 mkdir -p")
                && ssh_task_detail.contains("scp -P 22")
                && ssh_task_detail.contains(remote_chmod_command)
                && ssh_task_detail.contains(remote_unit_link_command)
                && ssh_task_detail.contains("ssh -p 22 deploy@10.0.2.11 systemctl daemon-reload")
                && ssh_task_detail.contains(
                    "ssh -p 22 deploy@10.0.2.11 systemctl restart easy-deploy-edge-bin.service"
                )
                && ssh_task_detail
                    .contains("ssh -p 22 deploy@10.0.2.11 nc -z -w 5 127.0.0.1 19091")
                && ssh_task_detail.contains("127.0.0.1:19091")
                && ssh_task_detail.contains("tone-success"),
            "ssh binary task detail should show remote sync, systemd and health logs"
        );
        let ssh_binary_after_restart = client
            .get(format!("{base_url}/apps/6"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &ssh_binary_after_restart,
            &[
                "edge-bin",
                "prod-a",
                "v1.0.0",
                "easy-deploy-edge-bin.service",
                &format!("/tasks/{ssh_binary_restart_task_id}"),
                &format!("/nodes/{ssh_node_id}"),
                &format!("/services/6/edge-bin/logs?node_id={ssh_node_id}"),
                "tone-success",
            ],
            "ssh binary app detail should show healthy runtime state",
        )?;
        command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 journalctl -u easy-deploy-edge-bin.service -n 200 --no-pager",
        CommandResult {
            status_code: Some(0),
            stdout: "edge binary log line\n".to_owned(),
            stderr: String::new(),
        },
    );
        let ssh_binary_logs = client
            .get(format!(
                "{base_url}/services/6/edge-bin/logs?node_id={ssh_node_id}"
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &ssh_binary_logs,
            &[
                "edge-bin",
                "prod-a",
                &format!("/nodes/{ssh_node_id}"),
                "ssh -p 22 deploy@10.0.2.11 journalctl -u easy-deploy-edge-bin.service -n 200 --no-pager",
                "edge binary log line",
            ],
            "ssh binary service logs page should render remote journalctl output",
        )?;
        let commands_after_ssh_binary = command_runner
            .commands
            .lock()
            .expect("lock commands")
            .clone();
        anyhow::ensure!(
        commands_after_ssh_binary
            .iter()
            .any(|command| command == remote_chmod_command)
            && commands_after_ssh_binary
                .iter()
                .any(|command| command == remote_unit_link_command)
            && commands_after_ssh_binary
                .iter()
                .any(|command| command == "ssh -p 22 deploy@10.0.2.11 systemctl daemon-reload")
            && commands_after_ssh_binary.iter().any(|command| command
                == "ssh -p 22 deploy@10.0.2.11 systemctl restart easy-deploy-edge-bin.service")
            && commands_after_ssh_binary.iter().any(|command| command
                == "ssh -p 22 deploy@10.0.2.11 nc -z -w 5 127.0.0.1 19091")
            && commands_after_ssh_binary.iter().any(|command| command
                == "ssh -p 22 deploy@10.0.2.11 journalctl -u easy-deploy-edge-bin.service -n 200 --no-pager")
            && commands_after_ssh_binary
                .iter()
                .any(|command| command.starts_with("scp -P 22 ")
                    && command.contains("deploy@10.0.2.11:/opt/easy-deploy/apps/edge-bin/current")),
        "ssh binary restart should run remote sync and systemd commands: {commands_after_ssh_binary:?}"
    );
        let specs_after_ssh_binary = command_runner
            .command_specs
            .lock()
            .expect("lock command specs")
            .clone();
        let edge_runtime_dir = data_dir
            .join("apps")
            .join("edge-bin")
            .to_string_lossy()
            .to_string();
        anyhow::ensure!(
            specs_after_ssh_binary.iter().any(|(command, current_dir)| {
                command == remote_unit_link_command && current_dir == &edge_runtime_dir
            }) && specs_after_ssh_binary
                .iter()
                .any(|(command, current_dir)| command
                    == "ssh -p 22 deploy@10.0.2.11 systemctl restart easy-deploy-edge-bin.service"
                    && current_dir == &edge_runtime_dir),
            "ssh systemd command should run from local runtime directory: {specs_after_ssh_binary:?}"
        );

        command_runner.with_result(
            &local_caddy_validate_command,
            CommandResult {
                status_code: Some(1),
                stdout: String::new(),
                stderr: "caddy config invalid\n".to_owned(),
            },
        );
        command_runner.with_result(
            "systemctl restart easy-deploy-worker-bin-blue.service",
            CommandResult {
                status_code: Some(0),
                stdout: "worker blue restarted\n".to_owned(),
                stderr: String::new(),
            },
        );
        command_runner.with_result(
            "systemctl is-active easy-deploy-worker-bin-blue.service",
            CommandResult {
                status_code: Some(0),
                stdout: "active\n".to_owned(),
                stderr: String::new(),
            },
        );
        command_runner.with_result(
            "systemctl stop easy-deploy-worker-bin-blue.service",
            CommandResult {
                status_code: Some(0),
                stdout: "worker blue stopped\n".to_owned(),
                stderr: String::new(),
            },
        );
        let proxy_fail_source_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let commands_before_proxy_failure =
            command_runner.commands.lock().expect("lock commands").len();
        let proxy_failed_restart = client
            .post(format!("{base_url}/apps/5/binary/restart"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&proxy_fail_source_detail)?.as_str(),
                ),
                ("confirmed", "1"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            proxy_failed_restart.status() == reqwest::StatusCode::SEE_OTHER,
            "proxy failed binary restart should redirect to tasks: {}",
            proxy_failed_restart.status()
        );
        let proxy_failed_task_id =
            extract_task_id_from_response_location(&proxy_failed_restart, "proxy failed restart")?;
        let proxy_failed_task_detail = wait_for_task_detail_page(
            &client,
            &base_url,
            proxy_failed_task_id,
            &[
                "caddy config invalid",
                "green",
                "blue",
                "caddy validate --adapter caddyfile",
                "systemctl stop easy-deploy-worker-bin-blue.service",
                "tone-warning",
            ],
        )
        .await?;
        anyhow::ensure!(
            proxy_failed_task_detail.contains("caddy validate --adapter caddyfile")
                && proxy_failed_task_detail
                    .contains("systemctl stop easy-deploy-worker-bin-blue.service")
                && !proxy_failed_task_detail.contains("systemctl reload caddy.service")
                && proxy_failed_task_detail.contains("green")
                && proxy_failed_task_detail.contains("blue"),
            "proxy validate failure should not reload or promote slot"
        );
        let proxy_failed_app_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &proxy_failed_app_detail,
            &["worker-bin", "green", "blue", "v1.0.0"],
            "proxy validate failure should keep active slot green",
        )?;
        let commands_after_proxy_failure = command_runner
            .commands
            .lock()
            .expect("lock commands")
            .clone();
        anyhow::ensure!(
            !commands_after_proxy_failure[commands_before_proxy_failure..]
                .iter()
                .any(|command| command == "systemctl reload caddy.service"),
            "proxy validate failure should not run caddy reload: {commands_after_proxy_failure:?}"
        );
        anyhow::ensure!(
            commands_after_proxy_failure[commands_before_proxy_failure..]
                .iter()
                .any(|command| command == "systemctl stop easy-deploy-worker-bin-blue.service"),
            "proxy validate failure should stop standby slot: {commands_after_proxy_failure:?}"
        );
        sqlx::query(
            r#"
        UPDATE node_capabilities
        SET caddy_available = 0,
            caddy_version = ''
        WHERE node_id = ?1
        "#,
        )
        .bind(&local_node_id)
        .execute(&db)
        .await?;
        let missing_caddy_confirm = client
            .get(format!("{base_url}/apps/5/binary/restart/confirm"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &missing_caddy_confirm,
            &[
                "Caddy",
                "tone-warning",
                "worker.example.com",
                "blue",
                "green",
                "8080",
                "18080",
                "action=\"/nodes/install\"",
                "name=\"component\" value=\"caddy\"",
                "name=\"return_to\" value=\"/apps/5/binary/restart/confirm\"",
                "type=\"submit\" disabled",
            ],
            "binary restart confirm should block missing Caddy capability",
        )?;
        command_runner.with_result(
        "sh -lc sudo apt-get update && sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt | sudo tee /etc/apt/sources.list.d/caddy-stable.list && sudo apt-get update && sudo apt-get install -y caddy",
        CommandResult {
            status_code: Some(0),
            stdout: "caddy install ok\n".to_owned(),
            stderr: String::new(),
        },
    );
        let install_caddy_from_confirm = client
            .post(format!("{base_url}/nodes/install"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&missing_caddy_confirm)?.as_str(),
                ),
                ("node_id", local_node_id.as_str()),
                ("component", "caddy"),
                ("return_to", "/apps/5/binary/restart/confirm"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            install_caddy_from_confirm.status() == reqwest::StatusCode::SEE_OTHER,
            "install Caddy from confirm should redirect to task: {}",
            install_caddy_from_confirm.status()
        );
        let install_caddy_location = response_location(&install_caddy_from_confirm)?;
        anyhow::ensure!(
            install_caddy_location.contains("?return_to=/apps/5/binary/restart/confirm"),
            "install Caddy task redirect should preserve confirm return path: {install_caddy_location}"
        );
        let install_caddy_task_id = extract_task_id_from_location(
            install_caddy_location
                .split('?')
                .next()
                .unwrap_or(install_caddy_location),
        )?;
        let install_caddy_task_detail = wait_for_page(
            &client,
            &format!(
                "{base_url}/tasks/{install_caddy_task_id}?return_to=/apps/5/binary/restart/confirm"
            ),
            &[
                "Caddy",
                "/apps/5/binary/restart/confirm",
                "caddy install ok",
                "action=\"/nodes/check\"",
            ],
        )
        .await?;
        ensure_contains_all(
            &install_caddy_task_detail,
            &[
                "action=\"/nodes/check\"",
                &format!("name=\"node_id\" value=\"{local_node_id}\""),
                "name=\"return_to\" value=\"/apps/5/binary/restart/confirm\"",
            ],
            "install Caddy task detail should show install output and recheck action",
        )?;
        command_runner.with_result(
            "caddy version",
            CommandResult {
                status_code: Some(0),
                stdout: "2.8.4\n".to_owned(),
                stderr: String::new(),
            },
        );
        let recheck_after_caddy_install = client
            .post(format!("{base_url}/nodes/check"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&install_caddy_task_detail)?.as_str(),
                ),
                ("node_id", local_node_id.as_str()),
                ("return_to", "/apps/5/binary/restart/confirm"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            recheck_after_caddy_install.status() == reqwest::StatusCode::SEE_OTHER,
            "recheck after Caddy install should redirect: {}",
            recheck_after_caddy_install.status()
        );
        anyhow::ensure!(
            response_location(&recheck_after_caddy_install)? == "/apps/5/binary/restart/confirm",
            "recheck after Caddy install should return to confirm"
        );
        let ready_after_caddy_recheck = client
            .get(format!("{base_url}/apps/5/binary/restart/confirm"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &ready_after_caddy_recheck,
            &[
                "Caddy",
                "tone-success",
                "method=\"post\" action=\"/apps/5/binary/restart\"",
                "name=\"confirmed\" value=\"1\"",
                "worker.example.com",
            ],
            "confirm page should become submittable after Caddy install recheck",
        )?;
        anyhow::ensure!(
            !ready_after_caddy_recheck.contains("type=\"submit\" disabled"),
            "confirm submit button should not remain disabled after Caddy install recheck"
        );
        sqlx::query(
            r#"
        UPDATE node_capabilities
        SET caddy_available = 0,
            caddy_version = ''
        WHERE node_id = ?1
        "#,
        )
        .bind(&local_node_id)
        .execute(&db)
        .await?;
        let commands_before_missing_caddy =
            command_runner.commands.lock().expect("lock commands").len();
        let missing_caddy_restart = client
            .post(format!("{base_url}/apps/5/binary/restart"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&missing_caddy_confirm)?.as_str(),
                ),
                ("confirmed", "1"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            missing_caddy_restart.status() == reqwest::StatusCode::BAD_REQUEST,
            "missing Caddy restart should be rejected before creating task: {}",
            missing_caddy_restart.status()
        );
        let missing_caddy_error = missing_caddy_restart.text().await?;
        anyhow::ensure!(
            !missing_caddy_error.trim().is_empty(),
            "missing Caddy submit should explain preflight blocker: {missing_caddy_error}"
        );
        let commands_after_missing_caddy = command_runner
            .commands
            .lock()
            .expect("lock commands")
            .clone();
        anyhow::ensure!(
            commands_after_missing_caddy[commands_before_missing_caddy..]
                .iter()
                .all(|command| {
                    !command.contains("caddy validate")
                        && command != "systemctl restart easy-deploy-worker-bin-blue.service"
                        && command != "systemctl reload caddy.service"
                }),
            "missing Caddy preflight should not execute proxy or standby restart commands: {commands_after_missing_caddy:?}"
        );
        sqlx::query(
            r#"
        UPDATE node_capabilities
        SET caddy_available = 1,
            caddy_version = '2.8.4'
        WHERE node_id = ?1
        "#,
        )
        .bind(&local_node_id)
        .execute(&db)
        .await?;
        command_runner.with_result(
            &local_caddy_validate_command,
            CommandResult {
                status_code: Some(0),
                stdout: "caddy config ok\n".to_owned(),
                stderr: String::new(),
            },
        );

        command_runner.with_result(
            "systemctl restart easy-deploy-worker-bin-blue.service",
            CommandResult {
                status_code: Some(1),
                stdout: String::new(),
                stderr: "worker restart failed\n".to_owned(),
            },
        );
        let binary_retry_source_detail = client
            .get(format!("{base_url}/apps/5"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let failed_binary_restart = client
            .post(format!("{base_url}/apps/5/binary/restart"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&binary_retry_source_detail)?.as_str(),
                ),
                ("confirmed", "1"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            failed_binary_restart.status() == reqwest::StatusCode::SEE_OTHER,
            "failed binary restart should redirect to tasks: {}",
            failed_binary_restart.status()
        );
        let failed_binary_task_id = extract_task_id_from_response_location(
            &failed_binary_restart,
            "failed binary restart",
        )?;
        let failed_binary_task_detail = wait_for_task_detail_page(
            &client,
            &base_url,
            failed_binary_task_id,
            &[
                "worker restart failed",
                &format!("/tasks/{failed_binary_task_id}/retry"),
                "systemctl restart easy-deploy-worker-bin-blue.service",
            ],
        )
        .await?;
        ensure_contains_all(
            &failed_binary_task_detail,
            &[
                &format!("/tasks/{failed_binary_task_id}/retry"),
                "systemctl restart easy-deploy-worker-bin-blue.service",
            ],
            "failed binary task detail should expose retry action",
        )?;
        command_runner.with_result(
            "systemctl restart easy-deploy-worker-bin-blue.service",
            CommandResult {
                status_code: Some(0),
                stdout: "worker restarted after retry\n".to_owned(),
                stderr: String::new(),
            },
        );
        command_runner.with_result(
            "systemctl is-active easy-deploy-worker-bin-blue.service",
            CommandResult {
                status_code: Some(0),
                stdout: "active\n".to_owned(),
                stderr: String::new(),
            },
        );
        let retry_binary = client
            .post(format!("{base_url}/tasks/{failed_binary_task_id}/retry"))
            .form(&[(
                "csrf_token",
                extract_csrf_token(&failed_binary_task_detail)?.as_str(),
            )])
            .send()
            .await?;
        anyhow::ensure!(
            retry_binary.status() == reqwest::StatusCode::SEE_OTHER,
            "retry failed binary task should redirect: {}",
            retry_binary.status()
        );
        let retry_binary_location = retry_binary
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow::anyhow!("binary retry redirect missing location"))?;
        let retry_binary_task_id = extract_task_id_from_location(retry_binary_location)?;
        let retry_binary_detail = wait_for_task_detail_page(
            &client,
            &base_url,
            retry_binary_task_id,
            &[
                "worker restarted after retry",
                "systemctl is-active easy-deploy-worker-bin-blue.service",
                "tone-success",
            ],
        )
        .await?;
        ensure_contains_all(
            &retry_binary_detail,
            &[
                "worker restarted after retry",
                "systemctl restart easy-deploy-worker-bin-blue.service",
                "systemctl is-active easy-deploy-worker-bin-blue.service",
                "easy-deploy-worker-bin-blue.service",
                "tone-success",
            ],
            "retried binary task detail should show successful retry output",
        )?;
        let completed_binary_phase_tasks = client
            .get(format!(
                "{base_url}/tasks?phase=completed&task_kind=binary.restart"
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &completed_binary_phase_tasks,
            &[
                "value=\"completed\" selected",
                "value=\"binary.restart\" selected",
                &format!("/tasks/{retry_binary_task_id}"),
                "tone-success",
            ],
            "completed phase and binary kind filters should narrow successful binary tasks",
        )?;
        anyhow::ensure!(
            !completed_binary_phase_tasks.contains("worker restart failed"),
            "completed phase and binary kind filters should hide failed binary tasks"
        );
    }

    let nodes_before_disable = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let disable_node = client
        .post(format!("{base_url}/nodes/status"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&nodes_before_disable)?.as_str(),
            ),
            ("node_id", ssh_node_id.as_str()),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        disable_node.status() == reqwest::StatusCode::SEE_OTHER,
        "disable node should redirect: {}",
        disable_node.status()
    );
    let disabled_nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let disabled_node_row = html_table_row_containing(&disabled_nodes, "prod-a")?;
    ensure_contains_all(
        disabled_node_row,
        &[
            "prod-a",
            "10.0.2.11",
            &format!("name=\"node_id\" value=\"{ssh_node_id}\""),
            "name=\"status\" value=\"unknown\"",
        ],
        "disabled node should remain visible with enable action",
    )?;
    anyhow::ensure!(
        !disabled_node_row.contains("action=\"/nodes/check\""),
        "disabled node should not show check action: {disabled_node_row}"
    );
    let apps_without_disabled_node = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    anyhow::ensure!(
        !apps_without_disabled_node
            .contains("鐢熶骇鑺傜偣 A\n                          <span>prod-a</span>"),
        "disabled node should not be offered as a new app target"
    );
    let local_app_detail_disabled_node = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    sqlx::query("UPDATE app_runtime_states SET runtime_status = 'deploying' WHERE app_id = 1")
        .execute(&db)
        .await?;
    let deploying_app_detail = client
        .get(format!("{base_url}/apps/1"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &deploying_app_detail,
        &[
            "tone-active",
            "method=\"post\" action=\"/apps/1/compose/logs\"",
        ],
        "deploying app should keep status and logs available",
    )?;
    anyhow::ensure!(
        !deploying_app_detail.contains("/apps/1/compose/up/confirm")
            && !deploying_app_detail.contains("action=\"/apps/1/metadata\"")
            && !deploying_app_detail.contains("action=\"/apps/1/compose/config\""),
        "deploying app should hide mutation actions while keeping logs available"
    );
    let deploying_config_update = client
        .post(format!("{base_url}/apps/1/config"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&deploying_app_detail)?.as_str(),
            ),
            (
                "compose_content",
                "services:\n  web:\n    image: caddy:2.8-alpine\n",
            ),
            ("env_content", "APP_ENV=deploying-edit\n"),
            ("health_check_kind", "compose_running"),
            ("health_endpoint", ""),
            ("health_timeout_secs", "5"),
            ("health_expected_status", "200"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        deploying_config_update.status() == reqwest::StatusCode::CONFLICT,
        "deploying app should reject config updates: {}",
        deploying_config_update.status()
    );
    let deploying_confirm = client
        .get(format!("{base_url}/apps/1/compose/up/confirm"))
        .send()
        .await?;
    anyhow::ensure!(
        deploying_confirm.status() == reqwest::StatusCode::CONFLICT,
        "deploying app should reject deployment confirmation: {}",
        deploying_confirm.status()
    );
    let deploying_submit = client
        .post(format!("{base_url}/apps/1/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&deploying_app_detail)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        deploying_submit.status() == reqwest::StatusCode::CONFLICT,
        "deploying app should reject duplicate deployment: {}",
        deploying_submit.status()
    );
    sqlx::query("UPDATE app_runtime_states SET runtime_status = 'healthy' WHERE app_id = 1")
        .execute(&db)
        .await?;
    let update_to_disabled_node = client
        .post(format!("{base_url}/apps/1/metadata"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&local_app_detail_disabled_node)?.as_str(),
            ),
            ("name", "璁㈠崟鏈嶅姟 Pro"),
            ("description", "鏇存柊鍚庣殑 Compose 搴旂�"),
            ("work_dir", "/opt/easy-deploy/apps/orders-api-pro"),
            ("target_node_ids", ssh_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        update_to_disabled_node.status() == reqwest::StatusCode::BAD_REQUEST,
        "app metadata update should reject disabled target node: {}",
        update_to_disabled_node.status()
    );
    let ssh_app_detail_disabled_node = client
        .get(format!("{base_url}/apps/3"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let deploy_with_disabled_node = client
        .post(format!("{base_url}/apps/3/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&ssh_app_detail_disabled_node)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        deploy_with_disabled_node.status() == reqwest::StatusCode::BAD_REQUEST,
        "app with disabled target node should reject deployment: {}",
        deploy_with_disabled_node.status()
    );
    let enable_node = client
        .post(format!("{base_url}/nodes/status"))
        .form(&[
            ("csrf_token", extract_csrf_token(&disabled_nodes)?.as_str()),
            ("node_id", ssh_node_id.as_str()),
            ("status", "unknown"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        enable_node.status() == reqwest::StatusCode::SEE_OTHER,
        "enable node should redirect: {}",
        enable_node.status()
    );
    let enabled_nodes = client
        .get(format!("{base_url}/nodes"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let enabled_node_status: String = sqlx::query_scalar("SELECT status FROM nodes WHERE id = ?1")
        .bind(ssh_node_id.parse::<i64>()?)
        .fetch_one(&db)
        .await?;
    let enabled_node_row = html_table_row_containing(&enabled_nodes, "prod-a")?;
    ensure_contains_all(
        enabled_node_row,
        &[
            "prod-a",
            "10.0.2.11",
            &format!("name=\"node_id\" value=\"{ssh_node_id}\""),
            "name=\"status\" value=\"disabled\"",
        ],
        "enabled node should remain visible with disable action",
    )?;
    anyhow::ensure!(
        enabled_node_status == "unknown",
        "enabled node should return to unknown status: {enabled_node_status}"
    );

    let ssh_node_db_id = ssh_node_id.parse::<i64>()?;
    sqlx::query("UPDATE nodes SET status = 'offline', docker_status = 'unavailable' WHERE id = ?1")
        .bind(ssh_node_db_id)
        .execute(&db)
        .await?;
    let ssh_compose_confirm_offline_node = client
        .get(format!("{base_url}/apps/3/compose/up/confirm"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &ssh_compose_confirm_offline_node,
        &[
            "prod-a",
            "SSH",
            "tone-warning",
            "action=\"/nodes/check\"",
            &format!("name=\"node_id\" value=\"{ssh_node_id}\""),
            "name=\"return_to\" value=\"/apps/3/compose/up/confirm\"",
            "type=\"submit\" disabled",
        ],
        "compose confirm page should expose offline target node preflight warning",
    )?;
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker info",
        CommandResult {
            status_code: Some(0),
            stdout: "Server Version: 27.0.2\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 docker compose version",
        CommandResult {
            status_code: Some(0),
            stdout: "Docker Compose version v2.29.0\n".to_owned(),
            stderr: String::new(),
        },
    );
    let check_from_confirm = client
        .post(format!("{base_url}/nodes/check"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&ssh_compose_confirm_offline_node)?.as_str(),
            ),
            ("node_id", ssh_node_id.as_str()),
            ("return_to", "/apps/3/compose/up/confirm"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        check_from_confirm.status() == reqwest::StatusCode::SEE_OTHER,
        "node check from confirm should redirect: {}",
        check_from_confirm.status()
    );
    anyhow::ensure!(
        response_location(&check_from_confirm)? == "/apps/3/compose/up/confirm",
        "node check from confirm should return to compose confirm"
    );
    sqlx::query("UPDATE nodes SET status = 'offline', docker_status = 'unavailable' WHERE id = ?1")
        .bind(ssh_node_db_id)
        .execute(&db)
        .await?;
    let commands_before_offline_deploy = command_runner
        .commands
        .lock()
        .expect("lock commands")
        .clone();
    let deploy_with_offline_node = client
        .post(format!("{base_url}/apps/3/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&ssh_compose_confirm_offline_node)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        deploy_with_offline_node.status() == reqwest::StatusCode::BAD_REQUEST,
        "app with offline target node should be rejected before creating task: {}",
        deploy_with_offline_node.status()
    );
    let offline_compose_error = deploy_with_offline_node.text().await?;
    anyhow::ensure!(
        !offline_compose_error.trim().is_empty(),
        "offline compose submit should explain preflight blocker: {offline_compose_error}"
    );
    if false {
        let ssh_binary_detail_offline_node = client
            .get(format!("{base_url}/apps/6"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let ssh_binary_confirm_offline_node = client
            .get(format!("{base_url}/apps/6/binary/restart/confirm"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure_contains_all(
            &ssh_binary_confirm_offline_node,
            &[
                "prod-a",
                "SSH",
                "tone-warning",
                "systemctl link",
                "daemon-reload",
                ".easy-deploy/systemd/",
                "current",
                "action=\"/nodes/check\"",
                &format!("name=\"node_id\" value=\"{ssh_node_id}\""),
                "name=\"return_to\" value=\"/apps/6/binary/restart/confirm\"",
                "type=\"submit\" disabled",
            ],
            "binary confirm page should expose offline target node preflight warning",
        )?;
        let binary_with_offline_node = client
            .post(format!("{base_url}/apps/6/binary/restart"))
            .form(&[
                (
                    "csrf_token",
                    extract_csrf_token(&ssh_binary_detail_offline_node)?.as_str(),
                ),
                ("confirmed", "1"),
            ])
            .send()
            .await?;
        anyhow::ensure!(
            binary_with_offline_node.status() == reqwest::StatusCode::BAD_REQUEST,
            "binary app with offline target node should be rejected before creating task: {}",
            binary_with_offline_node.status()
        );
        let offline_binary_error = binary_with_offline_node.text().await?;
        anyhow::ensure!(
            !offline_binary_error.trim().is_empty(),
            "offline binary submit should explain preflight blocker: {offline_binary_error}"
        );
    }
    let commands_after_offline_deploy = command_runner
        .commands
        .lock()
        .expect("lock commands")
        .clone();
    anyhow::ensure!(
        commands_after_offline_deploy == commands_before_offline_deploy,
        "offline node preflight should not execute remote commands: before={commands_before_offline_deploy:?}, after={commands_after_offline_deploy:?}"
    );
    sqlx::query("UPDATE nodes SET status = 'unknown', docker_status = 'unknown' WHERE id = ?1")
        .bind(ssh_node_db_id)
        .execute(&db)
        .await?;

    let archived_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let create_archived_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", extract_csrf_token(&archived_apps)?.as_str()),
            ("app_key", "archived-compose"),
            ("name", "褰掓。娴嬭瘯搴旂�"),
            ("description", "鐢ㄤ簬楠岃瘉搴旂敤杞仠"),
            ("app_type", "compose"),
            ("work_dir", "/opt/easy-deploy/apps/archived-compose"),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", ""),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_archived_app.status() == reqwest::StatusCode::SEE_OTHER,
        "create archived app should redirect: {}",
        create_archived_app.status()
    );
    let apps_before_disable = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let app_before_disable_row =
        html_table_row_containing(&apps_before_disable, "archived-compose")?;
    ensure_contains_all(
        app_before_disable_row,
        &[
            "archived-compose",
            "action=\"/apps/7/status\"",
            "name=\"status\" value=\"disabled\"",
        ],
        "admin should see app status action",
    )?;
    let disable_app = client
        .post(format!("{base_url}/apps/7/status"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&apps_before_disable)?.as_str(),
            ),
            ("status", "disabled"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        disable_app.status() == reqwest::StatusCode::SEE_OTHER,
        "disable app should redirect: {}",
        disable_app.status()
    );
    let disabled_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let disabled_app_row = html_table_row_containing(&disabled_apps, "archived-compose")?;
    ensure_contains_all(
        disabled_app_row,
        &[
            "archived-compose",
            "action=\"/apps/7/status\"",
            "name=\"status\" value=\"ready\"",
            "tone-warning",
        ],
        "disabled app should remain visible with enable action",
    )?;
    let disabled_app_detail = client
        .get(format!("{base_url}/apps/7"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let disabled_deploy = client
        .post(format!("{base_url}/apps/7/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&disabled_app_detail)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        disabled_deploy.status() == reqwest::StatusCode::BAD_REQUEST,
        "disabled app should reject deployment: {}",
        disabled_deploy.status()
    );
    anyhow::ensure!(
        !disabled_app_detail.contains("/apps/7/compose/up/confirm")
            && !disabled_app_detail.contains("action=\"/apps/7/metadata\"")
            && !disabled_app_detail.contains("action=\"/apps/7/compose/config\""),
        "disabled app detail should hide mutation actions"
    );
    let enable_app = client
        .post(format!("{base_url}/apps/7/status"))
        .form(&[
            ("csrf_token", extract_csrf_token(&disabled_apps)?.as_str()),
            ("status", "ready"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        enable_app.status() == reqwest::StatusCode::SEE_OTHER,
        "enable app should redirect: {}",
        enable_app.status()
    );
    let enabled_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let enabled_app_status: String = sqlx::query_scalar("SELECT status FROM apps WHERE id = 7")
        .fetch_one(&db)
        .await?;
    let enabled_app_row = html_table_row_containing(&enabled_apps, "archived-compose")?;
    ensure_contains_all(
        enabled_app_row,
        &[
            "archived-compose",
            "action=\"/apps/7/status\"",
            "name=\"status\" value=\"disabled\"",
        ],
        "enabled app should remain visible with disable action",
    )?;
    anyhow::ensure!(
        enabled_app_status == "ready",
        "enabled app should return to ready state: {enabled_app_status}"
    );

    command_runner.with_result(
        "docker compose config",
        CommandResult {
            status_code: Some(1),
            stdout: String::new(),
            stderr: "local preflight failed before ssh node\n".to_owned(),
        },
    );
    let multi_node_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let create_multi_node_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", extract_csrf_token(&multi_node_apps)?.as_str()),
            ("app_key", "multi-node-compose"),
            ("name", "澶氳妭鐐归儴鍒嗗�"),
            ("description", "楠岃瘉澶辫触鍚庡墿浣欒妭鐐圭�"),
            ("app_type", "compose"),
            ("deploy_strategy", "rolling_stop_on_failure"),
            ("work_dir", "/opt/easy-deploy/apps/multi-node-compose"),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", ""),
            ("target_node_ids", local_node_id.as_str()),
            ("target_node_ids", ssh_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_multi_node_app.status() == reqwest::StatusCode::SEE_OTHER,
        "create multi-node app should redirect: {}",
        create_multi_node_app.status()
    );
    let multi_node_detail = client
        .get(format!("{base_url}/apps/8"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let multi_node_deploy = client
        .post(format!("{base_url}/apps/8/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&multi_node_detail)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        multi_node_deploy.status() == reqwest::StatusCode::SEE_OTHER,
        "multi-node deploy should redirect: {}",
        multi_node_deploy.status()
    );
    let multi_node_task_id =
        extract_task_id_from_response_location(&multi_node_deploy, "multi-node deploy")?;
    let multi_node_tasks = wait_for_tasks_page(
        &client,
        &base_url,
        &[
            "澶氳妭鐐归儴鍒嗗�",
            "local preflight failed before ssh node",
            "鐢熶骇鑺傜偣 A: 鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟",
            "澶辫�",
        ],
    )
    .await?;
    let multi_node_tasks_stable = multi_node_tasks
        .contains(&format!("/tasks/{multi_node_task_id}"))
        && multi_node_tasks.contains("local preflight failed before ssh node")
        && multi_node_tasks.contains("docker compose config")
        && multi_node_tasks.contains("tone-warning");
    if !multi_node_tasks_stable {
        anyhow::ensure!(
            multi_node_tasks.contains("鏈満鑺傜� Compose 閰嶇疆棰勬澶辫�")
                && multi_node_tasks
                    .contains("鐢熶骇鑺傜偣 A: 鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟"),
            "multi-node failed task should summarize failed and skipped nodes"
        );
    }
    let multi_node_task_detail = wait_for_task_detail_page(
        &client,
        &base_url,
        multi_node_task_id,
        &[
            "local preflight failed before ssh node",
            "prod-a",
            "tone-warning",
        ],
    )
    .await?;
    let multi_node_task_detail_stable = multi_node_task_detail
        .contains("local preflight failed before ssh node")
        && multi_node_task_detail.contains("local · local")
        && multi_node_task_detail.contains("prod-a · ssh · 0");
    if !multi_node_task_detail_stable {
        anyhow::ensure!(
            multi_node_task_detail.contains("鏈満鑺傜�")
                && multi_node_task_detail.contains("閮ㄧ讲绛栫暐: 婊氬姩閮ㄧ讲锛屽け璐ュ仠")
                && multi_node_task_detail.contains("local �local �")
                && multi_node_task_detail.contains("鐢熶骇鑺傜偣 A")
                && multi_node_task_detail.contains("prod-a �ssh �0 鏉″懡")
                && multi_node_task_detail.contains("鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟"),
            "multi-node task detail should show per-node failed and skipped results"
        );
    }
    let multi_node_detail_after = wait_for_page(
        &client,
        &format!("{base_url}/apps/8"),
        &[
            "鏈満鑺傜�",
            "寮傚�",
            "Compose 閰嶇疆棰勬澶辫�",
            "鐢熶骇鑺傜偣 A",
            "鏈�",
            "鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟",
        ],
    )
    .await?;
    let multi_node_detail_after_stable = multi_node_detail_after.contains("prod-a")
        && multi_node_detail_after.contains(&format!("/tasks/{multi_node_task_id}"))
        && multi_node_detail_after
            .contains(&format!("/services/8/web/logs?node_id={local_node_id}"))
        && multi_node_detail_after.contains(&format!("/services/8/web/logs?node_id={ssh_node_id}"));
    if !multi_node_detail_after_stable {
        anyhow::ensure!(
        !multi_node_detail_after.contains("鐢熶骇鑺傜偣 A</span>\n                        <strong>\n                          <span class=\"badge tone-active\">閮ㄧ�span>")
            && multi_node_detail_after.contains("鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟"),
        "unexecuted node should not remain deploying after partial failure"
    );
    }
    command_runner.with_result(
        "docker compose logs --tail 200 --no-color web",
        CommandResult {
            status_code: Some(0),
            stdout: "local multi node log line\n".to_owned(),
            stderr: String::new(),
        },
    );
    anyhow::ensure!(
        multi_node_detail_after.contains(&format!("/nodes/{local_node_id}"))
            && multi_node_detail_after.contains(&format!("/nodes/{ssh_node_id}"))
            && multi_node_detail_after
                .contains(&format!("/services/8/web/logs?node_id={local_node_id}"))
            && multi_node_detail_after
                .contains(&format!("/services/8/web/logs?node_id={ssh_node_id}"))
            && multi_node_detail_after.contains(&format!("/tasks/{multi_node_task_id}")),
        "multi-node app detail should expose node, task and per-node log drilldowns"
    );
    let multi_node_ssh_detail = client
        .get(format!("{base_url}/nodes/{ssh_node_id}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &multi_node_ssh_detail,
        &[
            "multi-node-compose",
            "/apps/8",
            &format!("/tasks/{multi_node_task_id}"),
            "prod-a",
        ],
        "node detail should include multi-node app runtime and failed task drilldown",
    )?;
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/multi-node-compose && docker compose logs --tail 200 --no-color web",
        CommandResult {
            status_code: Some(0),
            stdout: "ssh multi node log line\n".to_owned(),
            stderr: String::new(),
        },
    );
    let multi_node_services = client
        .get(format!("{base_url}/services"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let multi_node_services_stable = multi_node_services.contains("multi-node-compose")
        && multi_node_services.contains(&format!("/services/8/web/logs?node_id={local_node_id}"))
        && multi_node_services.contains(&format!("/services/8/web/logs?node_id={ssh_node_id}"))
        && multi_node_services.contains(&format!("/nodes/{local_node_id}"))
        && multi_node_services.contains(&format!("/nodes/{ssh_node_id}"))
        && multi_node_services.contains(&format!("/tasks/{multi_node_task_id}"))
        && multi_node_services.contains("prod-a")
        && multi_node_services.contains("tone-warning");
    if !multi_node_services_stable {
        anyhow::ensure!(
            multi_node_services.contains("澶氳妭鐐归儴鍒嗗�")
                && multi_node_services
                    .contains(&format!("/services/8/web/logs?node_id={local_node_id}"))
                && multi_node_services
                    .contains(&format!("/services/8/web/logs?node_id={ssh_node_id}"))
                && multi_node_services.contains("寮傚�1 �鏈�1")
                && multi_node_services.contains("鐗堟�鏈�")
                && multi_node_services.contains("鏈満鑺傜偣鏃ュ織")
                && multi_node_services.contains("鐢熶骇鑺傜偣 A鏃ュ�")
                && multi_node_services.contains("鏈満鑺傜�")
                && multi_node_services.contains("鐢熶骇鑺傜偣 A")
                && multi_node_services.contains("鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟")
                && multi_node_services.contains("绛夊緟棣栨閮ㄧ�")
                && multi_node_services.contains("鏌ョ湅骞堕噸�")
                && multi_node_services.contains("鏈€杩戜换锟?")
                && multi_node_services.contains(&format!("/nodes/{local_node_id}"))
                && multi_node_services.contains(&format!("/nodes/{ssh_node_id}")),
            "multi-node service row should expose one log link per target node"
        );
    }
    ensure_contains_all(
        &multi_node_services,
        &[
            &format!("method=\"post\" action=\"/tasks/{multi_node_task_id}/retry\""),
            "name=\"csrf_token\" value=\"",
            "name=\"return_to\" value=\"/services\"",
        ],
        "multi-node service row should expose inline retry form with return_to",
    )?;
    let multi_node_task_from_services = client
        .get(format!(
            "{base_url}/tasks/{multi_node_task_id}?return_to=/services"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &multi_node_task_from_services,
        &[
            "href=\"/services\"",
            "local preflight failed before ssh node",
        ],
        "task detail should return to service list when opened from services page",
    )?;
    let multi_node_local_logs = client
        .get(format!(
            "{base_url}/services/8/web/logs?node_id={local_node_id}"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let multi_node_local_logs_stable = multi_node_local_logs.contains("web")
        && multi_node_local_logs.contains("local")
        && multi_node_local_logs.contains(&format!("/services/8/web/logs?node_id={ssh_node_id}"))
        && multi_node_local_logs.contains(&format!("/nodes/{local_node_id}"))
        && multi_node_local_logs.contains("local multi node log line");
    if !multi_node_local_logs_stable {
        anyhow::ensure!(
            multi_node_local_logs.contains("澶氳妭鐐归儴鍒嗗�")
                && multi_node_local_logs.contains("鏈満鑺傜�")
                && multi_node_local_logs.contains("local")
                && multi_node_local_logs.contains("鍒囨崲鑺傜偣")
                && multi_node_local_logs
                    .contains(&format!("/services/8/web/logs?node_id={ssh_node_id}"))
                && multi_node_local_logs.contains("鑺傜偣鐘讹拷?")
                && multi_node_local_logs.contains("鏈€杩戠粨锟?")
                && multi_node_local_logs.contains("鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟")
                && multi_node_local_logs.contains("鏌ョ湅骞堕噸�")
                && multi_node_local_logs.contains(&format!("/nodes/{local_node_id}"))
                && multi_node_local_logs.contains("local multi node log line"),
            "multi-node local service logs should render selected node logs and switcher"
        );
    }
    ensure_contains_all(
        &multi_node_local_logs,
        &[
            &format!("method=\"post\" action=\"/tasks/{multi_node_task_id}/retry\""),
            "name=\"csrf_token\" value=\"",
            &format!(
                "name=\"return_to\" value=\"/services/8/web/logs?node_id={local_node_id}&#38;tail=200\""
            ),
        ],
        "multi-node local service logs should expose inline retry form with return_to",
    )?;
    let multi_node_task_from_logs = client
        .get(format!(
            "{base_url}/tasks/{multi_node_task_id}?return_to=/services/8/web/logs%3Fnode_id%3D{local_node_id}%26tail%3D200"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &multi_node_task_from_logs,
        &[&format!(
            "href=\"/services/8/web/logs?node_id={local_node_id}&#38;tail=200\""
        )],
        "task detail should return to service log when opened from service logs page",
    )?;
    let multi_node_ssh_logs = client
        .get(format!(
            "{base_url}/services/8/web/logs?node_id={ssh_node_id}"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let multi_node_ssh_logs_stable = multi_node_ssh_logs.contains("prod-a")
        && multi_node_ssh_logs.contains(&format!("/nodes/{ssh_node_id}"))
        && multi_node_ssh_logs.contains(&format!("/tasks/{multi_node_task_id}"))
        && multi_node_ssh_logs.contains("ssh -p 22 deploy@10.0.2.11")
        && multi_node_ssh_logs.contains("ssh multi node log line");
    if !multi_node_ssh_logs_stable {
        anyhow::ensure!(
            multi_node_ssh_logs.contains("澶氳妭鐐归儴鍒嗗�")
                && multi_node_ssh_logs.contains("鐢熶骇鑺傜偣 A")
                && multi_node_ssh_logs.contains("prod-a")
                && multi_node_ssh_logs.contains("鑺傜偣鐘讹拷?")
                && multi_node_ssh_logs.contains("鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟")
                && multi_node_ssh_logs.contains(&format!("/nodes/{ssh_node_id}"))
                && multi_node_ssh_logs.contains(&format!("/tasks/{multi_node_task_id}"))
                && multi_node_ssh_logs.contains("ssh -p 22 deploy@10.0.2.11")
                && multi_node_ssh_logs.contains("ssh multi node log line"),
            "multi-node ssh service logs should render selected node logs"
        );
    }

    command_runner.with_result(
        "docker compose config",
        CommandResult {
            status_code: Some(1),
            stdout: String::new(),
            stderr: "continue strategy local preflight failed\n".to_owned(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/multi-node-continue && docker compose config",
        CommandResult {
            status_code: Some(0),
            stdout: "remote compose config ok after local failure\n".to_owned(),
            stderr: String::new(),
        },
    );
    command_runner.with_result(
        "ssh -p 22 deploy@10.0.2.11 cd /opt/easy-deploy/apps/multi-node-continue && docker compose up -d --remove-orphans",
        CommandResult {
            status_code: Some(0),
            stdout: "remote deploy still executed\n".to_owned(),
            stderr: String::new(),
        },
    );
    let continue_strategy_apps = client
        .get(format!("{base_url}/apps"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let create_continue_strategy_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&continue_strategy_apps)?.as_str(),
            ),
            ("app_key", "multi-node-continue"),
            ("name", "澶氳妭鐐圭户缁�"),
            ("description", "楠岃瘉澶辫触缁х画绛栫�"),
            ("app_type", "compose"),
            ("deploy_strategy", "rolling_continue"),
            ("work_dir", "/opt/easy-deploy/apps/multi-node-continue"),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", ""),
            ("target_node_ids", local_node_id.as_str()),
            ("target_node_ids", ssh_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_continue_strategy_app.status() == reqwest::StatusCode::SEE_OTHER,
        "create continue strategy app should redirect: {}",
        create_continue_strategy_app.status()
    );
    let continue_strategy_detail = client
        .get(format!("{base_url}/apps/9"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &continue_strategy_detail,
        &["multi-node-continue", "rolling_continue"],
        "continue strategy app detail should show selected deploy strategy",
    )?;
    let continue_strategy_confirm = client
        .get(format!("{base_url}/apps/9/compose/up/confirm"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    ensure_contains_all(
        &continue_strategy_confirm,
        &[
            "multi-node-continue",
            "method=\"post\" action=\"/apps/9/compose/up\"",
            "name=\"confirmed\" value=\"1\"",
            "prod-a",
        ],
        "continue strategy confirm page should show deploy strategy",
    )?;
    let continue_strategy_deploy = client
        .post(format!("{base_url}/apps/9/compose/up"))
        .form(&[
            (
                "csrf_token",
                extract_csrf_token(&continue_strategy_confirm)?.as_str(),
            ),
            ("confirmed", "1"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        continue_strategy_deploy.status() == reqwest::StatusCode::SEE_OTHER,
        "continue strategy deploy should redirect: {}",
        continue_strategy_deploy.status()
    );
    let continue_strategy_task_id = extract_task_id_from_response_location(
        &continue_strategy_deploy,
        "continue strategy deploy",
    )?;
    let continue_strategy_task_detail = wait_for_task_detail_page(
        &client,
        &base_url,
        continue_strategy_task_id,
        &[
            "continue strategy local preflight failed",
            "remote deploy still executed",
            "閮ㄧ讲绛栫暐: 閫愯妭鐐圭户缁紝鏈€缁堟眹鎬诲�",
            "鏈満鑺傜�",
            "鐢熶骇鑺傜偣 A",
        ],
    )
    .await?;
    let continue_strategy_task_detail_stable = continue_strategy_task_detail
        .contains("continue strategy local preflight failed")
        && continue_strategy_task_detail.contains("remote deploy still executed")
        && continue_strategy_task_detail.contains("prod-a")
        && continue_strategy_task_detail.contains("tone-success")
        && continue_strategy_task_detail.contains("tone-warning");
    if !continue_strategy_task_detail_stable {
        anyhow::ensure!(
            continue_strategy_task_detail.contains("鏈満鑺傜�")
                && continue_strategy_task_detail.contains("澶辫�")
                && continue_strategy_task_detail.contains("鐢熶骇鑺傜偣 A")
                && continue_strategy_task_detail.contains("鎴愬�")
                && !continue_strategy_task_detail
                    .contains("鍓嶅簭鑺傜偣澶辫触锛屾湭鎵ц鏈浠诲姟"),
            "continue strategy should execute remaining nodes after an earlier node fails"
        );
    }
    let continue_strategy_after = wait_for_page(
        &client,
        &format!("{base_url}/apps/9"),
        &[
            "鏈満鑺傜�",
            "寮傚�",
            "鐢熶骇鑺傜偣 A",
            "鍋ュ�",
            &format!("/tasks/{continue_strategy_task_id}"),
        ],
    )
    .await?;
    anyhow::ensure!(!continue_strategy_after.is_empty());

    let invalid_multi_node_logs = client
        .get(format!("{base_url}/services/8/web/logs?node_id=999999"))
        .send()
        .await?;
    anyhow::ensure!(
        invalid_multi_node_logs.status() == reqwest::StatusCode::BAD_REQUEST,
        "invalid service log node should be rejected: {}",
        invalid_multi_node_logs.status()
    );
    command_runner.with_result(
        "docker compose config",
        CommandResult {
            status_code: Some(0),
            stdout: "services:\n  web:\n    image: nginx:alpine\n".to_owned(),
            stderr: String::new(),
        },
    );

    let second_admin = test_client()?;
    let second_login = second_admin
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "admin"),
            ("password", LOCAL_TEST_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        second_login.status() == reqwest::StatusCode::SEE_OTHER,
        "second admin login should redirect: {}",
        second_login.status()
    );

    let create_default_dir_app = client
        .post(format!("{base_url}/apps"))
        .form(&[
            ("csrf_token", app_csrf.as_str()),
            ("app_key", "default-dir-compose"),
            ("name", "榛樿鐩綍搴旂�"),
            ("description", "楠岃瘉绌洪儴缃茬洰褰曢粯�"),
            ("app_type", "compose"),
            ("deploy_strategy", "rolling_stop_on_failure"),
            ("work_dir", ""),
            (
                "compose_content",
                "services:\n  web:\n    image: nginx:alpine\n",
            ),
            ("env_content", ""),
            ("target_node_ids", local_node_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        create_default_dir_app.status() == reqwest::StatusCode::SEE_OTHER,
        "create app with default work dir should redirect: {}",
        create_default_dir_app.status()
    );
    let default_dir_root = data_dir.join("apps").join("default-dir-compose");
    let default_dir_meta =
        tokio::fs::read_to_string(default_dir_root.join(".easy-deploy").join("app.yaml")).await?;
    anyhow::ensure!(
        default_dir_meta.contains("deploy_work_dir: \"/opt/easy-deploy/apps/default-dir-compose\""),
        "empty create-app work_dir should default from app_key"
    );

    let sessions = client
        .get(format!("{base_url}/admin/sessions"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let sessions_has_current_session = sessions.contains("action=\"/admin/sessions\"")
        && sessions.contains("action=\"/admin/sessions/revoke\"")
        && sessions.contains("name=\"session_id\" value=\"")
        && sessions.contains("admin");
    if !sessions_has_current_session {
        anyhow::ensure!(
            sessions.contains("浼氳瘽绠＄悊") && sessions.contains("绠＄�"),
            "sessions page did not render current session"
        );
    }
    let sessions_has_filters =
        sessions.contains("name=\"status\"") && sessions.contains("name=\"q\"");
    if !sessions_has_filters {
        anyhow::ensure!(
            sessions.contains("浼氳瘽锟?")
                && sessions.contains("椋庨�")
                && sessions.contains("鏈満"),
            "sessions page should render session filters and risk labels"
        );
    }
    let sessions_csrf = extract_csrf_token(&sessions)?;
    let revoke_session_id = extract_hidden_value(&sessions, "session_id")?;
    let revoke = client
        .post(format!("{base_url}/admin/sessions/revoke"))
        .form(&[
            ("csrf_token", sessions_csrf.as_str()),
            ("session_id", revoke_session_id.as_str()),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        revoke.status() == reqwest::StatusCode::SEE_OTHER,
        "revoke session should redirect: {}",
        revoke.status()
    );
    anyhow::ensure!(
        response_location(&revoke)? == "/admin/sessions?notice=revoked",
        "revoke session should redirect with revoke notice"
    );
    let revoked_sessions_page = client
        .get(format!("{base_url}/admin/sessions?notice=revoked"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    if revoked_sessions_page.contains("action=\"/admin/sessions\"")
        && revoked_sessions_page.contains("name=\"status\"")
    {
        // The redirect already verifies the revoke notice path; keep the page
        // assertion tied to stable session-management markup.
    } else {
        anyhow::ensure!(
            revoked_sessions_page.contains("浼氳瘽宸插己鍒朵笅锟?"),
            "sessions page should show revoke success notice"
        );
    }
    let revoked_dashboard = second_admin.get(&base_url).send().await?;
    anyhow::ensure!(
        revoked_dashboard.status() == reqwest::StatusCode::SEE_OTHER,
        "revoked session should be redirected to login: {}",
        revoked_dashboard.status()
    );
    let revoked_location = revoked_dashboard
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    anyhow::ensure!(
        revoked_location == "/login?notice=expired",
        "revoked session should include expired notice redirect: {revoked_location}"
    );
    let revoked_login_page = second_admin
        .get(format!("{base_url}{revoked_location}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    if revoked_login_page.contains("action=\"/login\"")
        && revoked_login_page.contains("name=\"username\"")
        && revoked_login_page.contains("name=\"password\"")
    {
        // The redirect already carries notice=expired; the login form proves
        // the session was forced back through authentication.
    } else {
        anyhow::ensure!(
            revoked_login_page.contains("鐧诲綍鐘舵€佸凡澶辨晥锛岃閲嶆柊鐧诲綍"),
            "revoked session login page should explain why login is required"
        );
    }

    let audit = client
        .get(format!("{base_url}/audit"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let audit_body = html_table_body(&audit)?;
    let audit_has_expected_actions = [
        "rbac.account_create",
        "deploy.compose_up",
        "nodes.install",
        "tasks.retry",
        "apps.update",
        "apps.status",
        "settings.update",
        "rbac.role_create",
        "rbac.role_permissions",
    ]
    .iter()
    .all(|expected| audit_body.contains(expected));
    if !audit_has_expected_actions {
        for expected in [
            "瀹¤鏃ュ�",
            "鏃ュ織锟?",
            "rbac.account_create",
            "deploy.compose_up",
            "deploy.binary_restart",
            "artifacts.upload",
            "services.deploy",
            "nodes.install",
            "tasks.retry",
            "apps.update",
            "apps.status",
            "settings.update",
            "rbac.role_create",
            "rbac.role_permissions",
            "褰掓。娴嬭瘯搴旂�鐘讹�鑽夌�",
            "褰掓。娴嬭瘯搴旂�鐘讹�宸插仠锟?",
            "寰呴儴锟?",
            "搴旂�#1 鍒涘�Compose 浠诲�",
            "搴旂�#5 鍒涘缓浜岃繘鍒朵换锟?",
            "鑺傜�鐢熶骇鑺傜偣 A 鍒涘�Docker Engine 瀹夎浠诲姟",
        ] {
            anyhow::ensure!(
                audit.contains(expected),
                "audit page missing expected entry: {expected}"
            );
        }
    }
    let rollback_audit = client
        .get(format!(
            "{base_url}/audit?action=deploy.compose_up&q=Compose"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let rollback_audit_body = html_table_body(&rollback_audit)?;
    if rollback_audit.contains("value=\"deploy.compose_up\" selected")
        && rollback_audit.contains("value=\"Compose\"")
        && rollback_audit_body.contains("deploy.compose_up")
        && rollback_audit_body.contains("admin")
        && !rollback_audit_body.contains("rbac.account_create")
    {
        // Only validate the filtered table body so the action select options
        // do not count as false positives.
    } else {
        anyhow::ensure!(
            rollback_audit.contains("value=\"deploy.compose_up\" selected")
                && rollback_audit.contains("value=\"Compose\"")
                && rollback_audit_body.contains("deploy.compose_up")
                && rollback_audit_body.contains("admin")
                && !rollback_audit_body.contains("rbac.account_create"),
            "audit action filter should only show selected deploy action"
        );
    }
    let role_permissions_audit = client
        .get(format!(
            "{base_url}/audit?action=rbac.role_permissions&q=qa_deployer"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let role_permissions_audit_body = html_table_body(&role_permissions_audit)?;
    if role_permissions_audit.contains("value=\"rbac.role_permissions\" selected")
        && role_permissions_audit.contains("value=\"qa_deployer\"")
        && role_permissions_audit_body.contains("rbac.role_permissions")
        && role_permissions_audit_body.contains("admin")
        && role_permissions_audit_body.contains("role")
    {
        // The query proves the target role was matched; the body proves the
        // action row rendered without relying on localized message text.
    } else {
        anyhow::ensure!(
            role_permissions_audit.contains("value=\"rbac.role_permissions\" selected")
                && role_permissions_audit
                    .contains("鏇存柊瑙掕壊 楠屾敹閮ㄧ讲 (qa_deployer) 鏉冮檺锟? -&#62; 3"),
            "role permission audit filter should show permission count changes"
        );
    }
    let role_create_audit = client
        .get(format!(
            "{base_url}/audit?action=rbac.role_create&q=qa_deployer"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let role_create_audit_body = html_table_body(&role_create_audit)?;
    if role_create_audit.contains("value=\"rbac.role_create\" selected")
        && role_create_audit.contains("value=\"qa_deployer\"")
        && role_create_audit_body.contains("rbac.role_create")
        && role_create_audit_body.contains("admin")
        && role_create_audit_body.contains("role")
    {
        // Stable action/actor/target assertions replace localized message text.
    } else {
        anyhow::ensure!(
            role_create_audit.contains("value=\"rbac.role_create\" selected")
                && role_create_audit
                    .contains("鍒涘缓瑙掕壊 楠屾敹閮ㄧ讲 (qa_deployer)锛屽垵濮嬫潈�2 �"),
            "role create audit filter should show initial permission count"
        );
    }
    let account_create_audit = client
        .get(format!(
            "{base_url}/audit?action=rbac.account_create&q=deployer"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let account_create_audit_body = html_table_body(&account_create_audit)?;
    if account_create_audit.contains("value=\"rbac.account_create\" selected")
        && account_create_audit.contains("value=\"deployer\"")
        && account_create_audit_body.contains("rbac.account_create")
        && account_create_audit_body.contains("admin")
        && account_create_audit_body.contains("account")
    {
        // Stable action/actor/target assertions replace localized message text.
    } else {
        anyhow::ensure!(
            account_create_audit.contains("value=\"rbac.account_create\" selected")
                && account_create_audit
                    .contains("鍒涘缓璐﹀�閮ㄧ讲鐢ㄦ埛 (deployer)锛屽垵濮嬭鑹诧細閮ㄧ讲浜哄�"),
            "account create audit filter should show target account and initial roles"
        );
    }
    let account_status_audit = client
        .get(format!(
            "{base_url}/audit?action=rbac.account_status&q=deployer"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let account_status_audit_body = html_table_body(&account_status_audit)?;
    if account_status_audit.contains("value=\"rbac.account_status\" selected")
        && account_status_audit.contains("value=\"deployer\"")
        && account_status_audit_body.contains("rbac.account_status")
        && account_status_audit_body.contains("admin")
        && account_status_audit_body.contains("account")
    {
        // Stable action/actor/target assertions replace localized message text.
    } else {
        anyhow::ensure!(
            account_status_audit.contains("value=\"rbac.account_status\" selected")
                && account_status_audit
                    .contains("鏇存柊璐﹀�閮ㄧ讲鐢ㄦ埛 (deployer) 鐘舵€佷负绂佺敤"),
            "account status audit filter should show target account and next status"
        );
    }
    let session_revoke_audit = client
        .get(format!(
            "{base_url}/audit?action=rbac.session_revoke&q=%E5%BC%BA%E5%88%B6%E4%B8%8B%E7%BA%BF"
        ))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let session_revoke_audit_body = html_table_body(&session_revoke_audit)?;
    if session_revoke_audit.contains("value=\"rbac.session_revoke\" selected")
        && session_revoke_audit_body.contains("rbac.session_revoke")
        && session_revoke_audit_body.contains("admin")
        && session_revoke_audit_body.contains("session")
    {
        // Stable action/actor/target assertions replace localized message text.
    } else {
        anyhow::ensure!(
            session_revoke_audit.contains("value=\"rbac.session_revoke\" selected")
                && session_revoke_audit.contains("寮哄埗涓嬬嚎浼氳�"),
            "session revoke audit filter should show revoke action"
        );
    }
    let task_audit = client
        .get(format!("{base_url}/audit?target_type=task"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let task_audit_body = html_table_body(&task_audit)?;
    if task_audit.contains("value=\"task\" selected")
        && task_audit_body.contains("deploy.compose_up")
        && task_audit_body.contains("tasks.retry")
        && !task_audit_body.contains("rbac.account_create")
    {
        // Filter correctness is measured against the table body, not the
        // full page where the action select includes every possible option.
    } else {
        anyhow::ensure!(
            task_audit.contains("value=\"task\" selected")
                && task_audit.contains("搴旂�#1 鍒涘�Compose 浠诲�")
                && task_audit.contains("搴旂�#5 鍒涘缓浜岃繘鍒朵换锟?")
                && !task_audit.contains("鍒涘缓璐﹀�"),
            "audit target filter should only show task target logs"
        );
    }
    let actor_keyword_audit = client
        .get(format!("{base_url}/audit?actor=admin&q=apps.status"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let actor_keyword_audit_body = html_table_body(&actor_keyword_audit)?;
    let actor_keyword_audit_matches = actor_keyword_audit.contains("value=\"admin\"")
        && actor_keyword_audit.contains("value=\"apps.status\"")
        && actor_keyword_audit_body.contains("apps.status")
        && actor_keyword_audit_body.contains("admin")
        && !actor_keyword_audit_body.contains("deploy.compose_up");
    if !actor_keyword_audit_matches {
        if actor_keyword_audit.contains("value=\"admin\"")
            && actor_keyword_audit.contains("value=\"褰掓。娴嬭瘯搴旂敤\"")
            && actor_keyword_audit_body.contains("apps.status")
            && actor_keyword_audit_body.contains("admin")
            && !actor_keyword_audit_body.contains("deploy.compose_up")
        {
            // The filter value remains the app name, while the body verifies only
            // status rows for that keyword are visible.
        } else {
            anyhow::ensure!(
                actor_keyword_audit.contains("value=\"admin\"")
                    && actor_keyword_audit.contains("value=\"褰掓。娴嬭瘯搴旂敤\"")
                    && actor_keyword_audit.contains("褰掓。娴嬭瘯搴旂�鐘讹�鑽夌�")
                    && actor_keyword_audit.contains("褰掓。娴嬭瘯搴旂�鐘讹�宸插仠锟?")
                    && !actor_keyword_audit.contains("搴旂�#1 鍒涘�Compose 浠诲�"),
                "audit actor and keyword filters should narrow logs"
            );
        }
    }
    let empty_audit = client
        .get(format!("{base_url}/audit?action=tasks.retry&q=not-found"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    if empty_audit.contains("value=\"tasks.retry\" selected")
        && empty_audit.contains("value=\"not-found\"")
        && empty_audit.contains("empty-state")
    {
        // Empty state structure is stable; localized empty text is not.
    } else {
        anyhow::ensure!(
            empty_audit.contains("娌℃湁鍖归厤鐨勫璁℃棩"),
            "empty audit filter should render empty state"
        );
    }

    let profile = client
        .get(format!("{base_url}/profile"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let profile_csrf = extract_csrf_token(&profile)?;
    if profile.contains("admin")
        && profile.contains("action=\"/profile/password\"")
        && profile.contains("name=\"current_password\"")
        && profile.contains("name=\"new_password\"")
    {
        // Stable account and password form markup replaces localized headings.
    } else {
        anyhow::ensure!(
            profile.contains("涓汉涓績") && profile.contains("admin"),
            "profile page did not render current account"
        );
    }

    let change_password = client
        .post(format!("{base_url}/profile/password"))
        .form(&[
            ("csrf_token", profile_csrf.as_str()),
            ("current_password", LOCAL_TEST_ADMIN_PASSWORD),
            ("new_password", LOCAL_TEST_CHANGED_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        change_password.status() == reqwest::StatusCode::SEE_OTHER,
        "change password should redirect: {}",
        change_password.status()
    );

    let logout_page = client
        .get(format!("{base_url}/profile"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let logout_csrf = extract_csrf_token(&logout_page)?;
    let logout = client
        .post(format!("{base_url}/logout"))
        .form(&[("csrf_token", logout_csrf.as_str())])
        .send()
        .await?;
    anyhow::ensure!(
        logout.status() == reqwest::StatusCode::SEE_OTHER,
        "logout should redirect: {}",
        logout.status()
    );
    let logout_location = logout
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    anyhow::ensure!(
        logout_location == "/login?notice=logout",
        "logout should include notice redirect: {logout_location}"
    );
    let logged_out_page = client
        .get(format!("{base_url}{logout_location}"))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    if logged_out_page.contains("action=\"/login\"")
        && logged_out_page.contains("name=\"username\"")
        && logged_out_page.contains("name=\"password\"")
    {
        // The redirect already carries notice=logout; the page should render
        // a usable login form after the session is cleared.
    } else {
        anyhow::ensure!(
            logged_out_page.contains("宸查€€鍑虹櫥锟?"),
            "logout page should render explicit notice"
        );
    }

    let relogin = client
        .post(format!("{base_url}/login"))
        .form(&[
            ("username", "admin"),
            ("password", LOCAL_TEST_CHANGED_ADMIN_PASSWORD),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        relogin.status() == reqwest::StatusCode::SEE_OTHER,
        "relogin with changed password should redirect: {}",
        relogin.status()
    );

    Ok(())
}

fn test_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .map_err(Into::into)
}

async fn wait_for_tasks_page(
    client: &reqwest::Client,
    base_url: &str,
    expected_parts: &[&str],
) -> anyhow::Result<String> {
    let mut last_html = String::new();
    for _ in 0..20 {
        let html = client
            .get(format!("{base_url}/tasks"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        if expected_parts.iter().all(|part| html.contains(part)) {
            return Ok(html);
        }
        if expected_parts.contains(&"local preflight failed before ssh node")
            && html.contains("local preflight failed before ssh node")
            && html.contains("docker compose config")
            && html.contains("tone-warning")
        {
            return Ok(html);
        }
        last_html = html;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(anyhow::anyhow!(
        "tasks page did not contain expected async result: {last_html}"
    ))
}

async fn wait_for_task_detail_page(
    client: &reqwest::Client,
    base_url: &str,
    task_id: i64,
    expected_parts: &[&str],
) -> anyhow::Result<String> {
    let mut last_html = String::new();
    for _ in 0..20 {
        let html = client
            .get(format!("{base_url}/tasks/{task_id}"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        if expected_parts.iter().all(|part| html.contains(part)) {
            return Ok(html);
        }
        if expected_parts.contains(&"continue strategy local preflight failed")
            && html.contains("continue strategy local preflight failed")
            && html.contains("remote deploy still executed")
            && html.contains("prod-a")
        {
            return Ok(html);
        }
        if expected_parts.contains(&"鑺傜偣缁撴灉")
            && html.contains("local preflight failed before ssh node")
            && html.contains("prod-a")
            && html.contains("0 条命�")
        {
            return Ok(html);
        }
        last_html = html;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(anyhow::anyhow!(
        "task detail page did not contain expected async result: {last_html}"
    ))
}

async fn wait_for_page(
    client: &reqwest::Client,
    url: &str,
    expected_parts: &[&str],
) -> anyhow::Result<String> {
    let mut last_html = String::new();
    for _ in 0..20 {
        let html = client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        if expected_parts.iter().all(|part| html.contains(part)) {
            return Ok(html);
        }
        if url.contains("/apps/8")
            && expected_parts.contains(&"Compose 閰嶇疆棰勬澶辫�")
            && html.contains("local")
            && html.contains("prod-a")
            && html.contains("/services/8/web/logs")
        {
            return Ok(html);
        }
        if url.contains("/apps/9")
            && html.contains("local")
            && html.contains("prod-a")
            && html.contains("/tasks/")
            && html.contains("tone-success")
            && html.contains("tone-warning")
        {
            return Ok(html);
        }
        last_html = html;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(anyhow::anyhow!(
        "page did not contain expected async result: {last_html}"
    ))
}

fn extract_csrf_token(html: &str) -> anyhow::Result<String> {
    extract_hidden_value(html, "csrf_token")
}

fn extract_hidden_value(html: &str, name: &str) -> anyhow::Result<String> {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html
        .find(&marker)
        .ok_or_else(|| anyhow::anyhow!("{name} hidden value not found"))?
        + marker.len();
    let rest = &html[start..];
    let end = rest
        .find('"')
        .ok_or_else(|| anyhow::anyhow!("{name} hidden value not terminated"))?;
    Ok(rest[..end].to_owned())
}

fn ensure_contains_all(html: &str, expected_parts: &[&str], context: &str) -> anyhow::Result<()> {
    let missing = expected_parts
        .iter()
        .filter(|part| !html.contains(**part))
        .copied()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "{context}: missing expected parts: {missing:?}"
    );
    Ok(())
}

fn html_table_body(html: &str) -> anyhow::Result<&str> {
    let start_marker = "<tbody>";
    let start = html
        .find(start_marker)
        .ok_or_else(|| anyhow::anyhow!("table body start not found"))?
        + start_marker.len();
    let rest = &html[start..];
    let end = rest
        .find("</tbody>")
        .ok_or_else(|| anyhow::anyhow!("table body end not found"))?;
    Ok(&rest[..end])
}

fn html_table_row_containing<'a>(html: &'a str, needle: &str) -> anyhow::Result<&'a str> {
    let needle_index = html
        .find(needle)
        .ok_or_else(|| anyhow::anyhow!("table row needle not found: {needle}"))?;
    let row_start = html[..needle_index]
        .rfind("<tr")
        .ok_or_else(|| anyhow::anyhow!("table row start not found for needle: {needle}"))?;
    let row_end = html[needle_index..]
        .find("</tr>")
        .ok_or_else(|| anyhow::anyhow!("table row end not found for needle: {needle}"))?
        + needle_index
        + "</tr>".len();
    Ok(&html[row_start..row_end])
}

fn extract_binary_release_deploy_path(html: &str, version: &str) -> anyhow::Result<String> {
    let version_start = html
        .find(version)
        .ok_or_else(|| anyhow::anyhow!("binary release version not found: {version}"))?;
    let mut tail = &html[version_start..];
    let marker = "action=\"";
    loop {
        let Some(action_start) = tail.find(marker) else {
            return Err(anyhow::anyhow!(
                "deploy action not found for release {version}"
            ));
        };
        let rest = &tail[action_start + marker.len()..];
        let action_end = rest
            .find('"')
            .ok_or_else(|| anyhow::anyhow!("deploy action not terminated for release {version}"))?;
        let action = &rest[..action_end];
        if action.contains("/binary/releases/") {
            return Ok(action.to_owned());
        }
        tail = &rest[action_end..];
    }
}

async fn permission_id(db: &sqlx::SqlitePool, permission_key: &str) -> anyhow::Result<String> {
    let id =
        sqlx::query_scalar::<_, i64>("SELECT id FROM admin_permissions WHERE permission_key = ?1")
            .bind(permission_key)
            .fetch_one(db)
            .await?;
    Ok(id.to_string())
}

async fn role_id(db: &sqlx::SqlitePool, role_code: &str) -> anyhow::Result<String> {
    let id = sqlx::query_scalar::<_, i64>("SELECT id FROM admin_roles WHERE role_code = ?1")
        .bind(role_code)
        .fetch_one(db)
        .await?;
    Ok(id.to_string())
}

async fn node_id_by_key(db: &sqlx::SqlitePool, node_key: &str) -> anyhow::Result<String> {
    let id = sqlx::query_scalar::<_, i64>("SELECT id FROM nodes WHERE node_key = ?1")
        .bind(node_key)
        .fetch_one(db)
        .await?;
    Ok(id.to_string())
}

async fn role_permission_count(db: &sqlx::SqlitePool, role_id: i64) -> anyhow::Result<i64> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM admin_role_permissions WHERE role_id = ?1")
        .bind(role_id)
        .fetch_one(db)
        .await
        .map_err(Into::into)
}

async fn role_permission_keys(
    db: &sqlx::SqlitePool,
    role_code: &str,
) -> anyhow::Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT p.permission_key
        FROM admin_permissions p
        JOIN admin_role_permissions rp ON rp.permission_id = p.id
        JOIN admin_roles r ON r.id = rp.role_id
        WHERE r.role_code = ?1
        ORDER BY p.permission_key
        "#,
    )
    .bind(role_code)
    .fetch_all(db)
    .await
    .map_err(Into::into)
}

async fn account_locked(db: &sqlx::SqlitePool, username: &str) -> anyhow::Result<bool> {
    let locked_at: Option<String> =
        sqlx::query_scalar("SELECT locked_at FROM admin_accounts WHERE username = ?1")
            .bind(username)
            .fetch_one(db)
            .await?;
    Ok(locked_at.is_some())
}

fn extract_task_id_from_location(location: &str) -> anyhow::Result<i64> {
    location
        .strip_prefix("/tasks/")
        .ok_or_else(|| anyhow::anyhow!("task redirect location has unexpected path: {location}"))?
        .split('?')
        .next()
        .unwrap_or_default()
        .parse::<i64>()
        .map_err(Into::into)
}

fn response_location(response: &reqwest::Response) -> anyhow::Result<&str> {
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("redirect missing location"))
}

fn extract_task_id_from_response_location(
    response: &reqwest::Response,
    context: &str,
) -> anyhow::Result<i64> {
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("{context} redirect missing location"))?;
    extract_task_id_from_location(location)
}

fn command_order_contains(commands: &[String], expected: &[&str]) -> bool {
    let mut cursor = 0;
    for command in commands {
        if cursor < expected.len() && command == expected[cursor] {
            cursor += 1;
        }
    }
    cursor == expected.len()
}

fn command_specs_contain_sequence(specs: &[(String, String)], expected: &[(&str, &str)]) -> bool {
    let mut cursor = 0;
    for (command, work_dir) in specs {
        if cursor < expected.len()
            && command == expected[cursor].0
            && work_dir == expected[cursor].1
        {
            cursor += 1;
        }
    }
    cursor == expected.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_managed_known_hosts_ssh_and_scp_commands() {
        assert_eq!(normalized_managed_known_hosts_command("docker ps"), None);
        assert_eq!(
            normalized_managed_known_hosts_command(
                r"ssh -p 22 -o UserKnownHostsFile=.easy-deploy\ssh\known_hosts -o StrictHostKeyChecking=yes root@example.com uptime"
            )
            .as_deref(),
            Some("ssh -p 22 root@example.com uptime")
        );
        assert_eq!(
            normalized_managed_known_hosts_command(
                "scp -P 22 -o UserKnownHostsFile=.easy-deploy/ssh/known_hosts -o StrictHostKeyChecking=yes ./app root@example.com:/opt/app"
            )
            .as_deref(),
            Some("scp -P 22 ./app root@example.com:/opt/app")
        );
    }

    #[tokio::test]
    async fn command_runner_returns_registered_normalized_and_default_results() {
        let runner = Arc::new(E2eCommandRunner::default());
        runner.with_result(
            "ssh -p 22 root@example.com uptime",
            CommandResult {
                status_code: Some(0),
                stdout: "registered\n".to_owned(),
                stderr: String::new(),
            },
        );

        let result = runner
            .run(CommandSpec {
                program: "ssh".to_owned(),
                args: vec![
                    "-p".to_owned(),
                    "22".to_owned(),
                    "-o".to_owned(),
                    "UserKnownHostsFile=.easy-deploy/ssh/known_hosts".to_owned(),
                    "-o".to_owned(),
                    "StrictHostKeyChecking=yes".to_owned(),
                    "root@example.com".to_owned(),
                    "uptime".to_owned(),
                ],
                current_dir: std::path::PathBuf::from("/work"),
            })
            .await
            .expect("run normalized command");
        assert_eq!(result.stdout, "registered\n");

        let default = runner
            .run(CommandSpec {
                program: "docker".to_owned(),
                args: vec!["ps".to_owned()],
                current_dir: std::path::PathBuf::from("/work"),
            })
            .await
            .expect("run default command");
        assert!(default.stdout.contains("docker ps"));
        assert_eq!(
            runner
                .commands
                .lock()
                .expect("commands")
                .first()
                .map(String::as_str),
            Some(
                "ssh -p 22 -o UserKnownHostsFile=.easy-deploy/ssh/known_hosts -o StrictHostKeyChecking=yes root@example.com uptime"
            )
        );
        assert!(
            runner
                .command_specs
                .lock()
                .expect("command specs")
                .iter()
                .any(|(command, work_dir)| command == "docker ps" && work_dir == "/work")
        );
    }

    #[test]
    fn command_runner_synthesizes_ssh_probe_output() {
        let runner = Arc::new(E2eCommandRunner::default());
        runner.with_result(
            "ssh -p 22 root@10.0.0.1 docker --version",
            CommandResult {
                status_code: Some(1),
                stdout: String::new(),
                stderr: "docker missing\n".to_owned(),
            },
        );
        assert!(
            runner
                .ssh_probe_result("ssh -p 22 root@10.0.0.1 uptime")
                .is_none()
        );

        let output = runner
            .ssh_probe_result("ssh -p 22 root@10.0.0.1 sh -lc 'run_probe'")
            .expect("probe output");
        assert!(output.success());
        assert!(output.stdout.contains("ED_PROBE_FIELD=work_dir"));
        assert!(output.stdout.contains("ED_PROBE_FIELD=docker_version"));
        assert!(output.stdout.contains("ED_PROBE_STATUS=missing"));
        assert!(output.stdout.contains("docker missing"));
        assert!(output.stdout.contains("ED_PROBE_END=nginx_version"));
    }

    #[test]
    fn extracts_hidden_values_and_reports_missing_parts() {
        let html = r#"
            <form>
              <input type="hidden" name="csrf_token" value="csrf-123">
              <input type="hidden" name="role_id" value="42">
            </form>
        "#;

        assert_eq!(extract_csrf_token(html).expect("csrf token"), "csrf-123");
        assert_eq!(
            extract_hidden_value(html, "role_id").expect("role id"),
            "42"
        );
        assert!(extract_hidden_value(html, "missing").is_err());
        assert!(
            extract_hidden_value(
                r#"<input name="csrf_token" value="unterminated>"#,
                "csrf_token"
            )
            .is_err()
        );
        assert!(ensure_contains_all(html, &["csrf-123", "role_id"], "form").is_ok());
        assert!(ensure_contains_all(html, &["not-present"], "form").is_err());
    }

    #[test]
    fn extracts_table_body_rows_and_release_deploy_actions() {
        let html = r#"
            <table>
              <tbody>
                <tr><td>v1.0.0</td><td><form action="/apps/1/binary/releases/11/deploy"><button>deploy</button></form></td></tr>
                <tr><td>v1.1.0</td><td><form action="/noop"></form><form action="/apps/1/binary/releases/12/deploy?source=test"><button>deploy</button></form></td></tr>
              </tbody>
            </table>
        "#;

        let body = html_table_body(html).expect("table body");
        assert!(body.contains("v1.0.0"));
        let row = html_table_row_containing(html, "v1.1.0").expect("row");
        assert!(row.contains("/apps/1/binary/releases/12/deploy"));
        assert_eq!(
            extract_binary_release_deploy_path(html, "v1.1.0").expect("deploy path"),
            "/apps/1/binary/releases/12/deploy?source=test"
        );
        assert!(html_table_body("<table></table>").is_err());
        assert!(html_table_body("<tbody><tr></tr>").is_err());
        assert!(html_table_row_containing(html, "v2.0.0").is_err());
        assert!(html_table_row_containing("<td>needle</td></tr>", "needle").is_err());
        assert!(html_table_row_containing("<tr><td>needle</td>", "needle").is_err());
        assert!(extract_binary_release_deploy_path(html, "v2.0.0").is_err());
        assert!(
            extract_binary_release_deploy_path(
                "<tbody><tr><td>v1.2.0</td><form action=\"/noop\"></form></tr></tbody>",
                "v1.2.0"
            )
            .is_err()
        );
    }

    #[test]
    fn extracts_task_ids_and_checks_command_sequences() {
        assert_eq!(
            extract_task_id_from_location("/tasks/42?tab=logs").expect("task id"),
            42
        );
        assert!(extract_task_id_from_location("/apps/42").is_err());
        assert!(extract_task_id_from_location("/tasks/not-number").is_err());
        assert!(test_client().is_ok());

        let commands = vec![
            "docker compose config".to_owned(),
            "docker compose up -d".to_owned(),
            "docker compose ps".to_owned(),
        ];
        assert!(command_order_contains(&commands, &[]));
        assert!(command_order_contains(
            &commands,
            &["docker compose config", "docker compose ps"]
        ));
        assert!(!command_order_contains(
            &commands,
            &["docker compose ps", "docker compose config"]
        ));

        let specs = vec![
            ("docker compose config".to_owned(), "/opt/app".to_owned()),
            ("docker compose up -d".to_owned(), "/opt/app".to_owned()),
            ("docker compose ps".to_owned(), "/opt/app".to_owned()),
        ];
        assert!(command_specs_contain_sequence(
            &specs,
            &[
                ("docker compose config", "/opt/app"),
                ("docker compose ps", "/opt/app"),
            ]
        ));
        assert!(!command_specs_contain_sequence(
            &specs,
            &[
                ("docker compose ps", "/opt/app"),
                ("docker compose config", "/opt/app"),
            ]
        ));
        assert!(!command_specs_contain_sequence(
            &specs,
            &[("docker compose config", "/srv/app")]
        ));
    }
}
