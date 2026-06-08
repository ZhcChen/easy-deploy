use std::{collections::HashMap, path::PathBuf};

use sqlx::SqlitePool;
use tracing::warn;

use crate::{
    deploy::{CommandSpec, DeployError, DynCommandRunner},
    tasks::{TaskNodeResultInput, TaskService},
};

#[derive(Clone)]
pub struct NodeService {
    db: SqlitePool,
    runner: DynCommandRunner,
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
    use tempfile::tempdir;

    use super::*;
    use crate::deploy::{CommandResult, CommandRunner};

    struct ProbeRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    impl ProbeRunner {
        fn new() -> Self {
            Self {
                specs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CommandRunner for ProbeRunner {
        async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
            let stdout = match (spec.program.as_str(), spec.args.as_slice()) {
                ("ssh", args)
                    if args
                        .last()
                        .is_some_and(|arg| arg.contains("ED_PROBE_STATUS")) =>
                {
                    "\
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=work_dir
/opt/easy-deploy/apps
ED_PROBE_END=work_dir
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=os_info
Linux 6.1 x86_64 GNU/Linux
ED_PROBE_END=os_info
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=disk_info
Filesystem      Size  Used Avail Use% Mounted on
/dev/sda1        40G  12G   28G  31% /
ED_PROBE_END=disk_info
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=systemd_version
systemd 252
+PAM +AUDIT
ED_PROBE_END=systemd_version
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=docker_version
Docker version 26.1.0
ED_PROBE_END=docker_version
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=docker_info
Server Version: 26.1.0
ED_PROBE_END=docker_info
ED_PROBE_STATUS=ok
ED_PROBE_FIELD=compose_version
Docker Compose version v2.27.0
ED_PROBE_END=compose_version
ED_PROBE_STATUS=missing
ED_PROBE_FIELD=caddy_version
caddy: command not found
ED_PROBE_END=caddy_version
ED_PROBE_STATUS=missing
ED_PROBE_FIELD=nginx_version
nginx: command not found
ED_PROBE_END=nginx_version
"
                }
                ("uname", _) => "Linux 6.1 x86_64 GNU/Linux\n",
                ("df", _) => {
                    "Filesystem      Size  Used Avail Use% Mounted on\n/dev/sda1        40G  12G   28G  31% /\n"
                }
                ("systemctl", _) => "systemd 252\n+PAM +AUDIT\n",
                ("docker", args) if args == ["--version"] => "Docker version 26.1.0\n",
                ("docker", args) if args == ["info"] => "Server Version: 26.1.0\n",
                ("docker", args) if args == ["compose", "version"] => {
                    "Docker Compose version v2.27.0\n"
                }
                ("caddy", args) if args == ["version"] => "2.8.4\n",
                ("nginx", args) if args == ["-v"] => "nginx version: nginx/1.24.0\n",
                _ => "ok\n",
            };
            self.specs.lock().expect("lock specs").push(spec);
            Ok(CommandResult {
                status_code: Some(0),
                stdout: stdout.to_owned(),
                stderr: String::new(),
            })
        }
    }

    async fn node_service(work_dir: PathBuf) -> (NodeService, Arc<ProbeRunner>) {
        let db = SqlitePool::connect_with(
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
        sqlx::query("UPDATE nodes SET work_dir = ?1 WHERE node_key = 'local'")
            .bind(work_dir.to_string_lossy().to_string())
            .execute(&db)
            .await
            .expect("set local work dir");
        let runner = Arc::new(ProbeRunner::new());
        (NodeService::new(db, runner.clone()), runner)
    }

    #[tokio::test]
    async fn check_node_updates_latest_capability_cache() {
        let work_dir = tempdir().expect("temp dir");
        let (service, _runner) = node_service(work_dir.path().to_path_buf()).await;
        let local_node_id: i64 =
            sqlx::query_scalar("SELECT id FROM nodes WHERE node_key = 'local'")
                .fetch_one(&service.db)
                .await
                .expect("read local node id");

        service
            .check_node(local_node_id)
            .await
            .expect("check local node");

        let node = service.fetch_node(local_node_id).await.expect("fetch node");
        assert_eq!(node.capability_status, "passed");
        assert_eq!(node.docker_available, 1);
        assert_eq!(node.compose_available, 1);
        assert_eq!(node.systemd_available, 1);
        assert_eq!(node.caddy_available, 1);
        assert_eq!(node.nginx_available, 1);
        assert_eq!(
            node.last_docker_version.as_deref(),
            Some("Docker version 26.1.0")
        );
        assert_eq!(
            node.last_compose_version.as_deref(),
            Some("Docker Compose version v2.27.0")
        );
        assert_eq!(node.last_caddy_version.as_deref(), Some("2.8.4"));
        assert_eq!(
            node.last_nginx_version.as_deref(),
            Some("nginx version: nginx/1.24.0")
        );

        let cached_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM node_capabilities WHERE node_id = ?1")
                .bind(local_node_id)
                .fetch_one(&service.db)
                .await
                .expect("count cached capabilities");
        assert_eq!(cached_count, 1);
    }

    #[tokio::test]
    async fn check_ssh_node_uses_bound_credential_identity_file() {
        let work_dir = tempdir().expect("temp dir");
        let identity_file = work_dir.path().join("id_ed25519");
        std::fs::write(&identity_file, "private").expect("write identity file");
        let (service, runner) = node_service(work_dir.path().to_path_buf()).await;
        let credential_id = sqlx::query(
            r#"
            INSERT INTO node_credentials(
                credential_key,
                name,
                public_key,
                private_key_path,
                fingerprint,
                status
            )
            VALUES ('test-key', '测试密钥', 'ssh-ed25519 AAAA', ?1, 'SHA256:test', 'active')
            "#,
        )
        .bind(identity_file.to_string_lossy().to_string())
        .execute(&service.db)
        .await
        .expect("insert credential")
        .last_insert_rowid();
        service
            .create_node(CreateNodeInput {
                node_key: "prod-a".to_owned(),
                name: "生产节点 A".to_owned(),
                node_type: "ssh".to_owned(),
                address: "10.0.2.11".to_owned(),
                ssh_port: 22,
                ssh_user: "deploy".to_owned(),
                credential_id: Some(credential_id),
                work_dir: "/opt/easy-deploy/apps".to_owned(),
                region: "prod".to_owned(),
                labels: "prod".to_owned(),
            })
            .await
            .expect("create ssh node");

        service.check_node(2).await.expect("check ssh node");

        let specs = runner.specs.lock().expect("lock specs");
        let ssh_specs = specs
            .iter()
            .filter(|spec| spec.program == "ssh")
            .collect::<Vec<_>>();
        assert_eq!(ssh_specs.len(), 1);
        let first_ssh = ssh_specs[0];
        let identity_arg = identity_file.to_string_lossy().to_string();
        assert_eq!(first_ssh.args[0], "-p");
        assert_eq!(first_ssh.args[1], "22");
        assert_eq!(first_ssh.args[2], "-o");
        assert_eq!(first_ssh.args[3], "BatchMode=yes");
        assert_eq!(first_ssh.args[4], "-o");
        assert_eq!(first_ssh.args[5], "ConnectTimeout=10");
        assert_eq!(first_ssh.args[6], "-o");
        assert_eq!(first_ssh.args[7], "ConnectionAttempts=3");
        assert_eq!(first_ssh.args[8], "-i");
        assert_eq!(first_ssh.args[9], identity_arg);
        assert_eq!(first_ssh.args[10], "-o");
        assert_eq!(first_ssh.args[11], "IdentitiesOnly=yes");
        assert_eq!(first_ssh.args[12], "deploy@10.0.2.11");
        assert_eq!(first_ssh.args[13], "sh");
        assert_eq!(first_ssh.args[14], "-lc");
        assert!(first_ssh.args[15].starts_with('\''));
        assert!(first_ssh.args[15].ends_with('\''));
        assert!(first_ssh.args[15].contains("/opt/easy-deploy/apps"));
    }
}

#[derive(Debug)]
pub enum NodeError {
    InvalidInput(String),
    Conflict(String),
    Internal(String),
}

impl NodeError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Conflict(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for NodeError {}

impl From<sqlx::Error> for NodeError {
    fn from(value: sqlx::Error) -> Self {
        if let sqlx::Error::Database(err) = &value
            && err.is_unique_violation()
        {
            return Self::Conflict("节点标识已存在".to_owned());
        }
        Self::Internal(format!("节点数据操作失败: {value}"))
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct NodeListItem {
    pub id: i64,
    pub node_key: String,
    pub name: String,
    pub node_type: String,
    pub address: String,
    pub ssh_port: i64,
    pub ssh_user: String,
    pub credential_id: Option<i64>,
    pub credential_name: Option<String>,
    pub credential_fingerprint: Option<String>,
    pub credential_private_key_path: Option<String>,
    pub work_dir: String,
    pub region: String,
    pub labels: String,
    pub status: String,
    pub docker_status: String,
    pub last_check_at: Option<String>,
    pub last_message: Option<String>,
    pub capability_status: String,
    pub docker_available: i64,
    pub compose_available: i64,
    pub systemd_available: i64,
    pub caddy_available: i64,
    pub nginx_available: i64,
    pub last_docker_version: Option<String>,
    pub last_compose_version: Option<String>,
    pub last_os_info: Option<String>,
    pub last_disk_info: Option<String>,
    pub last_systemd_version: Option<String>,
    pub last_caddy_version: Option<String>,
    pub last_nginx_version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CreateNodeInput {
    pub node_key: String,
    pub name: String,
    pub node_type: String,
    pub address: String,
    pub ssh_port: i64,
    pub ssh_user: String,
    pub credential_id: Option<i64>,
    pub work_dir: String,
    pub region: String,
    pub labels: String,
}

#[derive(Clone, Debug)]
pub struct UpdateNodeInput {
    pub node_id: i64,
    pub name: String,
    pub node_type: String,
    pub address: String,
    pub ssh_port: i64,
    pub ssh_user: String,
    pub credential_id: Option<i64>,
    pub work_dir: String,
    pub region: String,
    pub labels: String,
}

#[derive(Clone, Debug)]
pub struct NodeStatusChange {
    pub node_id: i64,
    pub node_name: String,
    pub previous_status: String,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeInstallComponent {
    Docker,
    Compose,
    Caddy,
    Nginx,
}

impl NodeInstallComponent {
    pub fn parse(value: &str) -> Result<Self, NodeError> {
        match value.trim() {
            "docker" => Ok(Self::Docker),
            "compose" => Ok(Self::Compose),
            "caddy" => Ok(Self::Caddy),
            "nginx" => Ok(Self::Nginx),
            _ => Err(NodeError::InvalidInput("节点安装组件不支持".to_owned())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Compose => "compose",
            Self::Caddy => "caddy",
            Self::Nginx => "nginx",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Docker => "Docker Engine",
            Self::Compose => "Docker Compose 插件",
            Self::Caddy => "Caddy",
            Self::Nginx => "Nginx",
        }
    }
}

pub struct NodeInstallResult {
    pub task_id: i64,
    pub node_name: String,
    pub component: NodeInstallComponent,
}

#[derive(Clone, Debug, Default)]
pub struct NodeCheckResult {
    pub status: String,
    pub message: String,
    pub docker_version: String,
    pub compose_version: String,
    pub os_info: String,
    pub disk_info: String,
    pub systemd_version: String,
    pub caddy_version: String,
    pub nginx_version: String,
}

#[derive(Clone, Debug)]
pub struct NodeDetail {
    pub node: NodeListItem,
    pub checks: Vec<NodeCheckHistoryItem>,
    pub apps: Vec<NodeAppRuntimeItem>,
    pub tasks: Vec<NodeTaskItem>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct NodeCheckHistoryItem {
    pub id: i64,
    pub check_status: String,
    pub message: String,
    pub docker_version: String,
    pub compose_version: String,
    pub os_info: String,
    pub disk_info: String,
    pub systemd_version: String,
    pub checked_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct NodeAppRuntimeItem {
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub app_type: String,
    pub app_status: String,
    pub runtime_status: String,
    pub active_version: String,
    pub service_count: i64,
    pub message: String,
    pub last_deploy_at: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct NodeTaskItem {
    pub id: i64,
    pub title: String,
    pub task_kind: String,
    pub app_name: String,
    pub status: String,
    pub phase: String,
    pub summary: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl NodeService {
    pub fn new(db: SqlitePool, runner: DynCommandRunner) -> Self {
        Self { db, runner }
    }

    pub async fn list_nodes(&self) -> Result<Vec<NodeListItem>, NodeError> {
        sqlx::query_as::<_, NodeListItem>(
            r#"
            SELECT
                n.id,
                n.node_key,
                n.name,
                n.node_type,
                n.address,
                n.ssh_port,
                n.ssh_user,
                n.credential_id,
                cred.name AS credential_name,
                cred.fingerprint AS credential_fingerprint,
                cred.private_key_path AS credential_private_key_path,
                n.work_dir,
                n.region,
                n.labels,
                n.status,
                n.docker_status,
                n.last_check_at,
                c.message AS last_message,
                COALESCE(c.check_status, 'unknown') AS capability_status,
                COALESCE(c.docker_available, 0) AS docker_available,
                COALESCE(c.compose_available, 0) AS compose_available,
                COALESCE(c.systemd_available, 0) AS systemd_available,
                COALESCE(c.caddy_available, 0) AS caddy_available,
                COALESCE(c.nginx_available, 0) AS nginx_available,
                c.docker_version AS last_docker_version,
                c.compose_version AS last_compose_version,
                c.os_info AS last_os_info,
                c.disk_info AS last_disk_info,
                c.systemd_version AS last_systemd_version,
                c.caddy_version AS last_caddy_version,
                c.nginx_version AS last_nginx_version
            FROM nodes n
            LEFT JOIN node_credentials cred ON cred.id = n.credential_id
            LEFT JOIN node_capabilities c ON c.node_id = n.id
            ORDER BY
                CASE n.node_key WHEN 'local' THEN 0 ELSE 1 END,
                n.id DESC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(NodeError::from)
    }

    pub async fn create_node(&self, input: CreateNodeInput) -> Result<(), NodeError> {
        let node_key = normalize_key(&input.node_key)?;
        let name = required_text(&input.name, "请输入节点名称")?;
        let node_type = normalize_node_type(&input.node_type)?;
        let address = required_text(&input.address, "请输入节点地址")?;
        let ssh_port = if node_type == "ssh" {
            validate_ssh_port(input.ssh_port)?
        } else {
            22
        };
        let ssh_user = if node_type == "ssh" {
            required_text(&input.ssh_user, "请输入 SSH 用户")?
        } else {
            String::new()
        };
        let credential_id = normalize_node_credential_id(input.credential_id, &node_type);
        if let Some(credential_id) = credential_id {
            ensure_active_credential(&self.db, credential_id).await?;
        }
        let work_dir = required_text(&input.work_dir, "请输入工作目录")?;
        let region = input.region.trim().to_owned();
        let labels = input.labels.trim().to_owned();

        sqlx::query(
            r#"
            INSERT INTO nodes(
                node_key,
                name,
                node_type,
                address,
                ssh_port,
                ssh_user,
                credential_id,
                work_dir,
                region,
                labels,
                status,
                docker_status
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'unknown', 'unknown')
            "#,
        )
        .bind(node_key)
        .bind(name)
        .bind(node_type)
        .bind(address)
        .bind(ssh_port)
        .bind(ssh_user)
        .bind(credential_id)
        .bind(work_dir)
        .bind(region)
        .bind(labels)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn node_detail(&self, node_id: i64) -> Result<NodeDetail, NodeError> {
        let node = self.fetch_node(node_id).await?;
        let checks = sqlx::query_as::<_, NodeCheckHistoryItem>(
            r#"
            SELECT
                id,
                check_status,
                message,
                docker_version,
                compose_version,
                os_info,
                disk_info,
                systemd_version,
                checked_at
            FROM node_checks
            WHERE node_id = ?1
            ORDER BY id DESC
            LIMIT 20
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.db)
        .await?;
        let apps = sqlx::query_as::<_, NodeAppRuntimeItem>(
            r#"
            SELECT
                a.id AS app_id,
                a.name AS app_name,
                a.app_key,
                a.app_type,
                a.status AS app_status,
                COALESCE(s.runtime_status, 'unknown') AS runtime_status,
                COALESCE(s.active_version, '') AS active_version,
                COALESCE(s.service_count, 0) AS service_count,
                COALESCE(s.message, '') AS message,
                s.last_deploy_at,
                COALESCE(s.updated_at, a.updated_at) AS updated_at
            FROM app_targets t
            JOIN apps a ON a.id = t.app_id
            LEFT JOIN app_runtime_states s
                ON s.app_id = t.app_id
               AND s.node_id = t.node_id
            WHERE t.node_id = ?1
            ORDER BY
                CASE COALESCE(s.runtime_status, 'unknown')
                    WHEN 'unhealthy' THEN 0
                    WHEN 'deploying' THEN 1
                    WHEN 'healthy' THEN 2
                    ELSE 3
                END,
                a.id DESC
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.db)
        .await?;
        let tasks = sqlx::query_as::<_, NodeTaskItem>(
            r#"
            SELECT DISTINCT
                t.id,
                t.title,
                t.task_kind,
                COALESCE(a.name, '') AS app_name,
                t.status,
                t.phase,
                t.summary,
                t.created_by,
                t.created_at,
                t.updated_at
            FROM operation_tasks t
            LEFT JOIN apps a ON a.id = t.app_id
            LEFT JOIN app_targets at ON at.app_id = t.app_id
            WHERE t.node_id = ?1
               OR at.node_id = ?1
            ORDER BY t.id DESC
            LIMIT 10
            "#,
        )
        .bind(node_id)
        .fetch_all(&self.db)
        .await?;
        Ok(NodeDetail {
            node,
            checks,
            apps,
            tasks,
        })
    }

    pub async fn update_node(&self, input: UpdateNodeInput) -> Result<(), NodeError> {
        let name = required_text(&input.name, "请输入节点名称")?;
        let node_type = normalize_node_type(&input.node_type)?;
        let address = required_text(&input.address, "请输入节点地址")?;
        let ssh_port = if node_type == "ssh" {
            validate_ssh_port(input.ssh_port)?
        } else {
            22
        };
        let ssh_user = if node_type == "ssh" {
            required_text(&input.ssh_user, "请输入 SSH 用户")?
        } else {
            String::new()
        };
        let credential_id = normalize_node_credential_id(input.credential_id, &node_type);
        if let Some(credential_id) = credential_id {
            ensure_active_credential(&self.db, credential_id).await?;
        }
        let work_dir = required_text(&input.work_dir, "请输入工作目录")?;
        let region = input.region.trim().to_owned();
        let labels = input.labels.trim().to_owned();

        let result = sqlx::query(
            r#"
            UPDATE nodes
            SET name = ?2,
                node_type = ?3,
                address = ?4,
                ssh_port = ?5,
                ssh_user = ?6,
                credential_id = ?7,
                work_dir = ?8,
                region = ?9,
                labels = ?10,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(input.node_id)
        .bind(name)
        .bind(node_type)
        .bind(address)
        .bind(ssh_port)
        .bind(ssh_user)
        .bind(credential_id)
        .bind(work_dir)
        .bind(region)
        .bind(labels)
        .execute(&self.db)
        .await?;
        if result.rows_affected() == 0 {
            return Err(NodeError::InvalidInput("节点不存在".to_owned()));
        }
        Ok(())
    }

    pub async fn set_node_status(
        &self,
        node_id: i64,
        status: &str,
    ) -> Result<NodeStatusChange, NodeError> {
        let status = match status {
            "disabled" | "unknown" => status,
            _ => return Err(NodeError::InvalidInput("节点状态不支持".to_owned())),
        };
        let node = self.fetch_node(node_id).await?;
        if node.status == status {
            return Ok(NodeStatusChange {
                node_id: node.id,
                node_name: node.name,
                previous_status: node.status.clone(),
                status: node.status,
            });
        }
        sqlx::query(
            r#"
            UPDATE nodes
            SET status = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(node.id)
        .bind(status)
        .execute(&self.db)
        .await?;
        Ok(NodeStatusChange {
            node_id: node.id,
            node_name: node.name,
            previous_status: node.status,
            status: status.to_owned(),
        })
    }

    pub async fn create_install_task(
        &self,
        tasks: &TaskService,
        node_id: i64,
        component: NodeInstallComponent,
        actor: &str,
    ) -> Result<NodeInstallResult, NodeError> {
        let node = self.fetch_node(node_id).await?;
        if node.status == "disabled" {
            return Err(NodeError::InvalidInput(
                "节点已禁用，不能安装组件".to_owned(),
            ));
        }
        let task_id = tasks
            .create_task(crate::tasks::CreateTaskInput {
                task_kind: format!("node.install.{}", component.as_str()),
                title: format!("安装 {} 到 {}", component.label(), node.name),
                app_id: None,
                node_id: Some(node.id),
                created_by: actor.to_owned(),
            })
            .await
            .map_err(|err| NodeError::Internal(err.message().to_owned()))?;
        tasks
            .append_log(task_id, "system", "节点组件安装任务已创建")
            .await
            .map_err(|err| NodeError::Internal(err.message().to_owned()))?;

        let node_name = node.name.clone();
        let service = self.clone();
        let task_service = tasks.clone();
        tokio::spawn(async move {
            service
                .run_install_task(task_service, task_id, node, component)
                .await;
        });

        Ok(NodeInstallResult {
            task_id,
            node_name,
            component,
        })
    }

    async fn run_install_task(
        self,
        tasks: TaskService,
        task_id: i64,
        node: NodeListItem,
        component: NodeInstallComponent,
    ) {
        let command = node_install_command(&node, component);
        let Ok(should_run) = tasks.mark_running(task_id, &command, "executing").await else {
            return;
        };
        if !should_run {
            return;
        }
        let output = run_node_install_command(&self.runner, &node, component).await;
        let (status, message, command_count) = match output {
            Ok(output) => {
                let command_count = 1;
                let status = if output.success() {
                    "success"
                } else {
                    "failed"
                };
                let message = node_install_summary(component, &output);
                let combined = output.combined_output();
                if !combined.trim().is_empty()
                    && let Err(err) = tasks.append_log(task_id, "combined", &combined).await
                {
                    warn!(task_id, error = %err, "failed to append node install output");
                }
                (status, message, command_count)
            }
            Err(err) => ("failed", err.message().to_owned(), 0),
        };
        if let Err(err) = tasks
            .record_node_result(TaskNodeResultInput {
                task_id,
                node_id: node.id,
                node_name: &node.name,
                node_key: &node.node_key,
                node_type: &node.node_type,
                status,
                message: &message,
                command_count,
            })
            .await
        {
            warn!(task_id, node_id = node.id, error = %err, "failed to record node install result");
        }
        if status == "success" {
            if let Err(err) = tasks.finish_success(task_id, &command, &message).await {
                warn!(task_id, error = %err, "failed to finish node install task");
            }
        } else if let Err(err) = tasks.fail_task(task_id, &message).await {
            warn!(task_id, error = %err, "failed to fail node install task");
        }
    }

    pub async fn check_node(&self, node_id: i64) -> Result<NodeCheckResult, NodeError> {
        let node = self.fetch_node(node_id).await?;
        if node.status == "disabled" {
            return Err(NodeError::InvalidInput(
                "节点已禁用，不能执行探测".to_owned(),
            ));
        }

        let result = if node.node_type == "local" {
            check_local_node(&self.runner, &node).await
        } else {
            check_ssh_node(&self.runner, &node).await
        };

        let node_status = if result.status == "passed" {
            "online"
        } else {
            "offline"
        };
        let docker_status = if result.status == "passed" {
            "available"
        } else if result.docker_version.is_empty() {
            "unknown"
        } else {
            "failed"
        };

        let mut tx = self.db.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO node_checks(
                node_id,
                check_status,
                message,
                docker_version,
                compose_version,
                os_info,
                disk_info,
                systemd_version
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
        )
        .bind(node_id)
        .bind(&result.status)
        .bind(&result.message)
        .bind(&result.docker_version)
        .bind(&result.compose_version)
        .bind(&result.os_info)
        .bind(&result.disk_info)
        .bind(&result.systemd_version)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE nodes
            SET status = ?2,
                docker_status = ?3,
                last_check_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(node_id)
        .bind(node_status)
        .bind(docker_status)
        .execute(&mut *tx)
        .await?;
        upsert_node_capabilities(&mut tx, node_id, &result).await?;
        tx.commit().await?;

        Ok(result)
    }

    async fn fetch_node(&self, node_id: i64) -> Result<NodeListItem, NodeError> {
        sqlx::query_as::<_, NodeListItem>(
            r#"
            SELECT
                n.id,
                n.node_key,
                n.name,
                n.node_type,
                n.address,
                n.ssh_port,
                n.ssh_user,
                n.credential_id,
                cred.name AS credential_name,
                cred.fingerprint AS credential_fingerprint,
                cred.private_key_path AS credential_private_key_path,
                n.work_dir,
                n.region,
                n.labels,
                n.status,
                n.docker_status,
                n.last_check_at,
                c.message AS last_message,
                COALESCE(c.check_status, 'unknown') AS capability_status,
                COALESCE(c.docker_available, 0) AS docker_available,
                COALESCE(c.compose_available, 0) AS compose_available,
                COALESCE(c.systemd_available, 0) AS systemd_available,
                COALESCE(c.caddy_available, 0) AS caddy_available,
                COALESCE(c.nginx_available, 0) AS nginx_available,
                c.docker_version AS last_docker_version,
                c.compose_version AS last_compose_version,
                c.os_info AS last_os_info,
                c.disk_info AS last_disk_info,
                c.systemd_version AS last_systemd_version,
                c.caddy_version AS last_caddy_version,
                c.nginx_version AS last_nginx_version
            FROM nodes n
            LEFT JOIN node_credentials cred ON cred.id = n.credential_id
            LEFT JOIN node_capabilities c ON c.node_id = n.id
            WHERE n.id = ?1
            "#,
        )
        .bind(node_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| NodeError::InvalidInput("节点不存在".to_owned()))
    }
}

async fn check_local_node(runner: &DynCommandRunner, node: &NodeListItem) -> NodeCheckResult {
    let work_dir = PathBuf::from(node.work_dir.trim());
    if let Err(err) = tokio::fs::create_dir_all(&work_dir).await {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: format!("本机工作目录不可用: {}，{err}", work_dir.to_string_lossy()),
            docker_version: String::new(),
            compose_version: String::new(),
            os_info: String::new(),
            disk_info: String::new(),
            systemd_version: String::new(),
            caddy_version: String::new(),
            nginx_version: String::new(),
        };
    }

    run_node_probe(
        NodeProbeTarget::Local {
            work_dir,
            executor_label: "本机",
        },
        runner,
    )
    .await
}

async fn check_ssh_node(runner: &DynCommandRunner, node: &NodeListItem) -> NodeCheckResult {
    let remote_work_dir = node.work_dir.trim();
    if !remote_work_dir.starts_with('/') {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: "SSH 节点工作目录必须是绝对路径".to_owned(),
            docker_version: String::new(),
            compose_version: String::new(),
            ..NodeCheckResult::default()
        };
    }
    if !is_safe_remote_probe_path(remote_work_dir) {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: "SSH 节点工作目录包含不支持的字符".to_owned(),
            docker_version: String::new(),
            compose_version: String::new(),
            ..NodeCheckResult::default()
        };
    }
    let destination = format!("{}@{}", node.ssh_user, node.address);
    run_ssh_node_probe(
        runner,
        SshProbeTarget {
            local_work_dir: PathBuf::from("."),
            destination,
            port: node.ssh_port,
            identity_file: node_identity_file(node),
            remote_work_dir: remote_work_dir.to_owned(),
        },
    )
    .await
}

async fn upsert_node_capabilities(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    node_id: i64,
    result: &NodeCheckResult,
) -> Result<(), NodeError> {
    let docker_available =
        i64::from(result.status == "passed" && !result.docker_version.is_empty());
    let compose_available =
        i64::from(result.status == "passed" && !result.compose_version.is_empty());
    let systemd_available =
        i64::from(result.status == "passed" && is_systemd_available(&result.systemd_version));
    let caddy_available = i64::from(result.status == "passed" && !result.caddy_version.is_empty());
    let nginx_available = i64::from(result.status == "passed" && !result.nginx_version.is_empty());
    sqlx::query(
        r#"
        INSERT INTO node_capabilities(
            node_id,
            check_status,
            message,
            docker_available,
            compose_available,
            systemd_available,
            caddy_available,
            nginx_available,
            docker_version,
            compose_version,
            os_info,
            disk_info,
            systemd_version,
            caddy_version,
            nginx_version,
            checked_at,
            updated_at
        )
        VALUES (
            ?1,
            ?2,
            ?3,
            ?4,
            ?5,
            ?6,
            ?7,
            ?8,
            ?9,
            ?10,
            ?11,
            ?12,
            ?13,
            ?14,
            ?15,
            strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        )
        ON CONFLICT(node_id) DO UPDATE SET
            check_status = excluded.check_status,
            message = excluded.message,
            docker_available = excluded.docker_available,
            compose_available = excluded.compose_available,
            systemd_available = excluded.systemd_available,
            caddy_available = excluded.caddy_available,
            nginx_available = excluded.nginx_available,
            docker_version = excluded.docker_version,
            compose_version = excluded.compose_version,
            os_info = excluded.os_info,
            disk_info = excluded.disk_info,
            systemd_version = excluded.systemd_version,
            caddy_version = excluded.caddy_version,
            nginx_version = excluded.nginx_version,
            checked_at = excluded.checked_at,
            updated_at = excluded.updated_at
        "#,
    )
    .bind(node_id)
    .bind(&result.status)
    .bind(&result.message)
    .bind(docker_available)
    .bind(compose_available)
    .bind(systemd_available)
    .bind(caddy_available)
    .bind(nginx_available)
    .bind(&result.docker_version)
    .bind(&result.compose_version)
    .bind(&result.os_info)
    .bind(&result.disk_info)
    .bind(&result.systemd_version)
    .bind(&result.caddy_version)
    .bind(&result.nginx_version)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn is_systemd_available(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.contains(':')
}

enum NodeProbeTarget {
    Local {
        work_dir: PathBuf,
        executor_label: &'static str,
    },
}

struct SshProbeTarget {
    local_work_dir: PathBuf,
    destination: String,
    port: i64,
    identity_file: Option<PathBuf>,
    remote_work_dir: String,
}

async fn run_node_probe(target: NodeProbeTarget, runner: &DynCommandRunner) -> NodeCheckResult {
    let executor_label = target.executor_label();
    if let Err(err) = prepare_probe_work_dir(runner, &target).await {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: err,
            docker_version: String::new(),
            compose_version: String::new(),
            ..NodeCheckResult::default()
        };
    }

    let os_info = probe_command(runner, &target, &["uname", "-srmo"])
        .await
        .unwrap_or_else(|err| format!("OS 探测失败: {err}"));
    let disk_info = probe_command(runner, &target, &["df", "-h", target.work_dir()])
        .await
        .unwrap_or_else(|err| format!("磁盘探测失败: {err}"));
    let systemd_version = probe_command(runner, &target, &["systemctl", "--version"])
        .await
        .map(|output| first_non_empty_line(&output))
        .unwrap_or_else(|err| format!("systemd 探测失败: {err}"));

    let docker_version = match probe_command(runner, &target, &["docker", "--version"]).await {
        Ok(output) => output,
        Err(err) => {
            return NodeCheckResult {
                status: "failed".to_owned(),
                message: format!("{executor_label} Docker CLI 不可用: {err}"),
                docker_version: String::new(),
                compose_version: String::new(),
                os_info,
                disk_info,
                systemd_version,
                caddy_version: String::new(),
                nginx_version: String::new(),
            };
        }
    };
    if let Err(err) = probe_command(runner, &target, &["docker", "info"]).await {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: format!("{executor_label} Docker daemon 不可用: {err}"),
            docker_version,
            compose_version: String::new(),
            os_info,
            disk_info,
            systemd_version,
            caddy_version: String::new(),
            nginx_version: String::new(),
        };
    }
    let compose_version =
        match probe_command(runner, &target, &["docker", "compose", "version"]).await {
            Ok(output) => output,
            Err(err) => {
                return NodeCheckResult {
                    status: "failed".to_owned(),
                    message: format!("{executor_label} Docker Compose 不可用: {err}"),
                    docker_version,
                    compose_version: String::new(),
                    os_info,
                    disk_info,
                    systemd_version,
                    caddy_version: String::new(),
                    nginx_version: String::new(),
                };
            }
        };
    let caddy_version = optional_probe_version(runner, &target, &["caddy", "version"]).await;
    let nginx_version = optional_probe_version(runner, &target, &["nginx", "-v"]).await;

    NodeCheckResult {
        status: "passed".to_owned(),
        message: format!("{executor_label} 节点探测通过，Docker 与 Compose 可用"),
        docker_version,
        compose_version,
        os_info,
        disk_info,
        systemd_version,
        caddy_version,
        nginx_version,
    }
}

async fn run_ssh_node_probe(runner: &DynCommandRunner, target: SshProbeTarget) -> NodeCheckResult {
    let script = ssh_probe_script(&target.remote_work_dir);
    let mut args = vec!["-p".to_owned(), target.port.to_string()];
    append_ssh_probe_options(&mut args);
    append_ssh_identity_args(&mut args, target.identity_file.as_ref());
    args.extend([
        target.destination,
        "sh".to_owned(),
        "-lc".to_owned(),
        shell_quote(&script),
    ]);
    let rendered_command = render_probe_command("ssh", &args);
    let result = match runner
        .run(CommandSpec {
            program: "ssh".to_owned(),
            args,
            current_dir: target.local_work_dir,
        })
        .await
    {
        Ok(result) => result,
        Err(err) => {
            return NodeCheckResult {
                status: "failed".to_owned(),
                message: format!("SSH 连接失败: {}", deploy_error_message(&err)),
                docker_version: String::new(),
                compose_version: String::new(),
                ..NodeCheckResult::default()
            };
        }
    };
    if !result.success() {
        let output = result.combined_output();
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: if output.is_empty() {
                format!("{rendered_command} 退出码 {:?}", result.status_code)
            } else {
                format!("{rendered_command}: {output}")
            },
            docker_version: String::new(),
            compose_version: String::new(),
            ..NodeCheckResult::default()
        };
    }

    ssh_probe_result_from_output(&result.combined_output())
}

fn ssh_probe_result_from_output(output: &str) -> NodeCheckResult {
    let sections = parse_ssh_probe_sections(output);
    if let Err(err) = require_probe_section_status(&sections, "work_dir") {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: err,
            docker_version: String::new(),
            compose_version: String::new(),
            ..NodeCheckResult::default()
        };
    }
    let os_info = probe_section_output(&sections, "os_info")
        .unwrap_or_else(|| "OS 探测失败: 未返回结果".to_owned());
    let disk_info = probe_section_output(&sections, "disk_info")
        .unwrap_or_else(|| "磁盘探测失败: 未返回结果".to_owned());
    let systemd_version = probe_section_output(&sections, "systemd_version")
        .map(|value| first_non_empty_line(&value))
        .unwrap_or_else(|| "systemd 探测失败: 未返回结果".to_owned());
    let docker_version = match require_probe_section(&sections, "docker_version") {
        Ok(value) => value,
        Err(err) => {
            return NodeCheckResult {
                status: "failed".to_owned(),
                message: format!("SSH Docker CLI 不可用: {err}"),
                docker_version: String::new(),
                compose_version: String::new(),
                os_info,
                disk_info,
                systemd_version,
                caddy_version: String::new(),
                nginx_version: String::new(),
            };
        }
    };
    if let Err(err) = require_probe_section(&sections, "docker_info") {
        return NodeCheckResult {
            status: "failed".to_owned(),
            message: format!("SSH Docker daemon 不可用: {err}"),
            docker_version,
            compose_version: String::new(),
            os_info,
            disk_info,
            systemd_version,
            caddy_version: String::new(),
            nginx_version: String::new(),
        };
    }
    let compose_version = match require_probe_section(&sections, "compose_version") {
        Ok(value) => value,
        Err(err) => {
            return NodeCheckResult {
                status: "failed".to_owned(),
                message: format!("SSH Docker Compose 不可用: {err}"),
                docker_version,
                compose_version: String::new(),
                os_info,
                disk_info,
                systemd_version,
                caddy_version: String::new(),
                nginx_version: String::new(),
            };
        }
    };
    let caddy_version = probe_section_output(&sections, "caddy_version")
        .map(|value| first_non_empty_line(&value))
        .unwrap_or_default();
    let nginx_version = probe_section_output(&sections, "nginx_version")
        .map(|value| first_non_empty_line(&value))
        .unwrap_or_default();

    NodeCheckResult {
        status: "passed".to_owned(),
        message: "SSH 节点探测通过，Docker 与 Compose 可用".to_owned(),
        docker_version,
        compose_version,
        os_info,
        disk_info,
        systemd_version,
        caddy_version,
        nginx_version,
    }
}

impl NodeProbeTarget {
    fn executor_label(&self) -> &'static str {
        match self {
            Self::Local { executor_label, .. } => executor_label,
        }
    }

    fn work_dir(&self) -> &str {
        match self {
            Self::Local { work_dir, .. } => work_dir.to_str().unwrap_or("."),
        }
    }
}

async fn prepare_probe_work_dir(
    _runner: &DynCommandRunner,
    target: &NodeProbeTarget,
) -> Result<(), String> {
    match target {
        NodeProbeTarget::Local { .. } => Ok(()),
    }
}

async fn probe_command(
    runner: &DynCommandRunner,
    target: &NodeProbeTarget,
    command: &[&str],
) -> Result<String, String> {
    let (program, args, current_dir) = match target {
        NodeProbeTarget::Local { work_dir, .. } => {
            let Some((program, args)) = command.split_first() else {
                return Err("探测命令为空".to_owned());
            };
            (
                (*program).to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                work_dir.clone(),
            )
        }
    };
    let rendered_command = render_probe_command(&program, &args);
    let result = runner
        .run(CommandSpec {
            program,
            args,
            current_dir,
        })
        .await
        .map_err(|err| deploy_error_message(&err))?;
    if result.success() {
        let output = result.combined_output();
        Ok(if output.is_empty() {
            rendered_command
        } else {
            output
        })
    } else {
        let output = result.combined_output();
        Err(if output.is_empty() {
            format!("{rendered_command} 退出码 {:?}", result.status_code)
        } else {
            format!("{rendered_command}: {output}")
        })
    }
}

async fn run_node_install_command(
    runner: &DynCommandRunner,
    node: &NodeListItem,
    component: NodeInstallComponent,
) -> Result<crate::deploy::CommandResult, DeployError> {
    let script = node_install_script(component);
    if node.node_type == "local" {
        let work_dir = PathBuf::from(node.work_dir.trim());
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|err| DeployError::Command(format!("准备本机工作目录失败: {err}")))?;
        return runner
            .run(CommandSpec {
                program: "sh".to_owned(),
                args: vec!["-lc".to_owned(), script.to_owned()],
                current_dir: work_dir,
            })
            .await;
    }

    let destination = format!("{}@{}", node.ssh_user, node.address);
    let mut args = vec!["-p".to_owned(), node.ssh_port.to_string()];
    append_ssh_probe_options(&mut args);
    append_ssh_identity_args(&mut args, node_identity_file(node).as_ref());
    args.extend([
        destination,
        "sh".to_owned(),
        "-lc".to_owned(),
        shell_quote(script),
    ]);
    runner
        .run(CommandSpec {
            program: "ssh".to_owned(),
            args,
            current_dir: PathBuf::from("."),
        })
        .await
}

fn node_install_command(node: &NodeListItem, component: NodeInstallComponent) -> String {
    let script = node_install_script(component);
    if node.node_type == "ssh" {
        let mut args = vec!["-p".to_owned(), node.ssh_port.to_string()];
        append_ssh_probe_options(&mut args);
        append_ssh_identity_args(&mut args, node_identity_file(node).as_ref());
        args.extend([
            format!("{}@{}", node.ssh_user, node.address),
            "sh".to_owned(),
            "-lc".to_owned(),
            shell_quote(script),
        ]);
        format!("ssh {}", args.join(" "))
    } else {
        format!("sh -lc {script}")
    }
}

fn node_identity_file(node: &NodeListItem) -> Option<PathBuf> {
    node.credential_private_key_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn append_ssh_identity_args(args: &mut Vec<String>, identity_file: Option<&PathBuf>) {
    if let Some(identity_file) = identity_file {
        args.push("-i".to_owned());
        args.push(identity_file.to_string_lossy().to_string());
        args.push("-o".to_owned());
        args.push("IdentitiesOnly=yes".to_owned());
    }
}

fn append_ssh_probe_options(args: &mut Vec<String>) {
    args.push("-o".to_owned());
    args.push("BatchMode=yes".to_owned());
    args.push("-o".to_owned());
    args.push("ConnectTimeout=10".to_owned());
    args.push("-o".to_owned());
    args.push("ConnectionAttempts=3".to_owned());
}

fn node_install_script(component: NodeInstallComponent) -> &'static str {
    match component {
        NodeInstallComponent::Docker => {
            "curl -fsSL https://get.docker.com | sudo sh && sudo systemctl enable --now docker"
        }
        NodeInstallComponent::Compose => {
            "sudo apt-get update && sudo apt-get install -y docker-compose-plugin"
        }
        NodeInstallComponent::Caddy => {
            "sudo apt-get update && sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg && curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt | sudo tee /etc/apt/sources.list.d/caddy-stable.list && sudo apt-get update && sudo apt-get install -y caddy"
        }
        NodeInstallComponent::Nginx => {
            "sudo apt-get update && sudo apt-get install -y nginx && sudo systemctl enable --now nginx"
        }
    }
}

fn node_install_summary(
    component: NodeInstallComponent,
    output: &crate::deploy::CommandResult,
) -> String {
    if output.success() {
        return format!("{} 安装命令执行成功，请重新探测节点能力", component.label());
    }
    let combined_output = output.combined_output();
    if combined_output.trim().is_empty() {
        format!(
            "{} 安装命令失败，退出码 {:?}",
            component.label(),
            output.status_code
        )
    } else {
        format!(
            "{} 安装命令失败: {}",
            component.label(),
            first_non_empty_line(&combined_output)
        )
    }
}

async fn optional_probe_version(
    runner: &DynCommandRunner,
    target: &NodeProbeTarget,
    command: &[&str],
) -> String {
    probe_command(runner, target, command)
        .await
        .map(|output| first_non_empty_line(&output))
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default)]
struct SshProbeSection {
    status: String,
    output: String,
}

fn parse_ssh_probe_sections(output: &str) -> HashMap<String, SshProbeSection> {
    let mut sections = HashMap::new();
    let mut current_status: Option<String> = None;
    let mut current_field: Option<String> = None;
    let mut current_output = Vec::new();

    for line in output.lines() {
        if let Some(status) = line.strip_prefix("ED_PROBE_STATUS=") {
            current_status = Some(status.trim().to_owned());
            continue;
        }
        if let Some(field) = line.strip_prefix("ED_PROBE_FIELD=") {
            current_field = Some(field.trim().to_owned());
            current_output.clear();
            continue;
        }
        if line.starts_with("ED_PROBE_END=") {
            if let Some(field) = current_field.take() {
                sections.insert(
                    field,
                    SshProbeSection {
                        status: current_status.take().unwrap_or_else(|| "error".to_owned()),
                        output: current_output.join("\n").trim().to_owned(),
                    },
                );
            }
            current_output.clear();
            continue;
        }
        if current_field.is_some() {
            current_output.push(line.to_owned());
        }
    }

    sections
}

fn probe_section_output(
    sections: &HashMap<String, SshProbeSection>,
    field: &str,
) -> Option<String> {
    sections
        .get(field)
        .filter(|section| section.status == "ok")
        .map(|section| section.output.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn require_probe_section(
    sections: &HashMap<String, SshProbeSection>,
    field: &str,
) -> Result<String, String> {
    match sections.get(field) {
        Some(section) if section.status == "ok" && !section.output.trim().is_empty() => {
            Ok(section.output.trim().to_owned())
        }
        Some(section) if !section.output.trim().is_empty() => Err(format!(
            "{} 探测失败: {}",
            probe_field_label(field),
            first_non_empty_line(&section.output)
        )),
        Some(_) => Err(format!("{} 探测失败", probe_field_label(field))),
        None => Err(format!("{} 探测未返回结果", probe_field_label(field))),
    }
}

fn require_probe_section_status(
    sections: &HashMap<String, SshProbeSection>,
    field: &str,
) -> Result<(), String> {
    match sections.get(field) {
        Some(section) if section.status == "ok" => Ok(()),
        Some(section) if !section.output.trim().is_empty() => Err(format!(
            "{} 探测失败: {}",
            probe_field_label(field),
            first_non_empty_line(&section.output)
        )),
        Some(_) => Err(format!("{} 探测失败", probe_field_label(field))),
        None => Err(format!("{} 探测未返回结果", probe_field_label(field))),
    }
}

fn probe_field_label(field: &str) -> &'static str {
    match field {
        "work_dir" => "SSH 工作目录",
        "docker_version" => "Docker CLI",
        "docker_info" => "Docker daemon",
        "compose_version" => "Docker Compose",
        _ => "命令",
    }
}

fn ssh_probe_script(remote_work_dir: &str) -> String {
    format!(
        r#"run_probe() {{
  field="$1"
  shift
  tmp="$(mktemp)"
  if "$@" >"$tmp" 2>&1; then
    status="ok"
  else
    status="missing"
  fi
  printf 'ED_PROBE_STATUS=%s\n' "$status"
  printf 'ED_PROBE_FIELD=%s\n' "$field"
  cat "$tmp"
  printf '\nED_PROBE_END=%s\n' "$field"
  rm -f "$tmp"
}}
run_probe work_dir mkdir -p {remote_work_dir}
run_probe os_info uname -srmo
run_probe disk_info df -h {remote_work_dir}
run_probe systemd_version systemctl --version
run_probe docker_version docker --version
run_probe docker_info docker info
run_probe compose_version docker compose version
run_probe caddy_version caddy version
run_probe nginx_version nginx -v
"#
    )
}

fn render_probe_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_owned()
    } else {
        format!("{} {}", program, args.join(" "))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

fn deploy_error_message(err: &DeployError) -> String {
    err.message().to_owned()
}

fn first_non_empty_line(value: &str) -> String {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn normalize_key(value: &str) -> Result<String, NodeError> {
    let key = value.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err(NodeError::InvalidInput("请输入节点标识".to_owned()));
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(NodeError::InvalidInput(
            "节点标识仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    Ok(key)
}

fn normalize_node_type(value: &str) -> Result<String, NodeError> {
    let node_type = value.trim().to_ascii_lowercase();
    match node_type.as_str() {
        "local" | "ssh" => Ok(node_type),
        _ => Err(NodeError::InvalidInput("节点类型不支持".to_owned())),
    }
}

fn required_text(value: &str, message: &str) -> Result<String, NodeError> {
    let value = value.trim();
    if value.is_empty() {
        Err(NodeError::InvalidInput(message.to_owned()))
    } else {
        Ok(value.to_owned())
    }
}

fn validate_ssh_port(port: i64) -> Result<i64, NodeError> {
    if (1..=65535).contains(&port) {
        Ok(port)
    } else {
        Err(NodeError::InvalidInput(
            "SSH 端口需要在 1 到 65535 之间".to_owned(),
        ))
    }
}

fn normalize_node_credential_id(credential_id: Option<i64>, node_type: &str) -> Option<i64> {
    if node_type == "ssh" {
        credential_id.filter(|id| *id > 0)
    } else {
        None
    }
}

fn is_safe_remote_probe_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.contains("//")
        && !value.split('/').any(|part| part == "." || part == "..")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '@'))
}

async fn ensure_active_credential(db: &SqlitePool, credential_id: i64) -> Result<(), NodeError> {
    let status =
        sqlx::query_scalar::<_, String>("SELECT status FROM node_credentials WHERE id = ?1")
            .bind(credential_id)
            .fetch_optional(db)
            .await?;
    match status.as_deref() {
        Some("active") => Ok(()),
        Some(_) => Err(NodeError::InvalidInput("节点凭据已禁用".to_owned())),
        None => Err(NodeError::InvalidInput("节点凭据不存在".to_owned())),
    }
}
