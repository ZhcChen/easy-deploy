use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use tar::Archive;
use tokio::fs;

use crate::{
    application_config::{ApplicationConfigService, ConfigUnit},
    deploy::{ComposeCommandOutput, ComposeExecutor, SshExecutor, SshTarget},
    deployment_orchestrator::{DeploymentAction, UnitExecutionContext},
    health::{HealthCheckKind, normalize_health_config},
};

#[derive(Clone)]
pub struct DeploymentRuntimeService {
    db: SqlitePool,
    configs: ApplicationConfigService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentRuntimeError {
    Validation(String),
    NotFound(String),
    Config(String),
    Database(String),
}

impl std::fmt::Display for DeploymentRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::NotFound(message)
            | Self::Config(message)
            | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DeploymentRuntimeError {}

impl From<sqlx::Error> for DeploymentRuntimeError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnitExecutionSpec {
    pub app_id: i64,
    pub app_key: String,
    pub environment_id: i64,
    pub environment_key: String,
    pub config_revision_id: i64,
    pub config_hash: String,
    pub unit_id: i64,
    pub unit_key: String,
    pub unit: ConfigUnit,
    pub action: DeploymentAction,
    pub release: Option<UnitReleaseSpec>,
    pub target_nodes: Vec<DeploymentTargetNode>,
    pub environment_variables: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitReleaseSpec {
    pub id: i64,
    pub version: String,
    pub version_code: i64,
    pub package_name: String,
    pub package_path: PathBuf,
    pub checksum_sha256: String,
    pub size_bytes: i64,
    pub storage_provider: String,
    pub storage_bucket: String,
    pub storage_object_key: String,
    pub storage_endpoint: String,
    pub storage_object_version_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedUnitRuntime {
    pub root: PathBuf,
    pub compose_path: PathBuf,
    pub env_path: PathBuf,
    pub package_path: Option<PathBuf>,
    pub script_paths: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Clone)]
pub struct UnitNodeExecutionResult {
    pub success: bool,
    pub summary: String,
    pub outputs: Vec<ComposeCommandOutput>,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct DeploymentTargetNode {
    pub id: i64,
    pub node_key: String,
    pub name: String,
    pub node_type: String,
    pub address: String,
    pub ssh_port: i64,
    pub ssh_user: String,
    pub credential_private_key_path: Option<String>,
    pub work_dir: String,
    pub status: String,
    pub docker_status: String,
}

#[derive(Debug, FromRow)]
struct ExecutionIdentityRow {
    app_id: i64,
    app_key: String,
    environment_key: String,
    config_revision_id: i64,
}

#[derive(Debug, FromRow)]
struct UnitReleaseRow {
    id: i64,
    version: String,
    version_code: i64,
    package_name: String,
    package_path: String,
    checksum_sha256: String,
    size_bytes: i64,
    storage_provider: String,
    storage_bucket: String,
    storage_object_key: String,
    storage_endpoint: String,
    storage_object_version_id: String,
}

impl DeploymentRuntimeService {
    pub fn new(db: SqlitePool, configs: ApplicationConfigService) -> Self {
        Self { db, configs }
    }

    pub async fn load_unit_spec(
        &self,
        context: &UnitExecutionContext,
    ) -> Result<UnitExecutionSpec, DeploymentRuntimeError> {
        let identity = sqlx::query_as::<_, ExecutionIdentityRow>(
            r#"
            SELECT runs.app_id, apps.app_key, environments.environment_key,
                   runs.config_revision_id
            FROM environment_deployment_runs runs
            JOIN apps ON apps.id = runs.app_id
            JOIN app_environments environments ON environments.id = runs.environment_id
            WHERE runs.id = ?1 AND runs.task_id = ?2 AND runs.environment_id = ?3
            "#,
        )
        .bind(context.deployment_run_id)
        .bind(context.task_id)
        .bind(context.environment_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| Self::not_found("deployment execution identity not found"))?;
        let revision = self
            .configs
            .load_revision(identity.app_id, identity.config_revision_id)
            .await
            .map_err(|error| DeploymentRuntimeError::Config(error.to_string()))?;
        let unit = revision
            .document
            .units
            .into_iter()
            .find(|unit| unit.key == context.item.unit_key)
            .ok_or_else(|| Self::validation("deployment unit is missing from config revision"))?;
        let persisted_unit_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM deployment_units WHERE id = ?1 AND app_id = ?2 AND unit_key = ?3",
        )
        .bind(context.item.unit_id)
        .bind(identity.app_id)
        .bind(&context.item.unit_key)
        .fetch_optional(&self.db)
        .await?;
        if persisted_unit_id.is_none() {
            return Err(Self::validation(
                "deployment unit identity does not belong to application",
            ));
        }
        let release = match context.item.unit_release_id {
            Some(release_id) => Some(self.load_release(context.item.unit_id, release_id).await?),
            None if context.item.action == DeploymentAction::Stop => None,
            None => {
                return Err(Self::validation(
                    "deployment action requires a unit release",
                ));
            }
        };
        let target_nodes = self
            .load_target_nodes(context.environment_id, &context.target_node_ids)
            .await?;
        let environment_variables = environment_variables(
            &revision.secret_values,
            &identity.environment_key,
            &context.item.unit_key,
        );
        Ok(UnitExecutionSpec {
            app_id: identity.app_id,
            app_key: identity.app_key,
            environment_id: context.environment_id,
            environment_key: identity.environment_key,
            config_revision_id: revision.revision_id,
            config_hash: revision.config_hash,
            unit_id: context.item.unit_id,
            unit_key: context.item.unit_key.clone(),
            unit,
            action: context.item.action,
            release,
            target_nodes,
            environment_variables,
        })
    }

    async fn load_release(
        &self,
        unit_id: i64,
        release_id: i64,
    ) -> Result<UnitReleaseSpec, DeploymentRuntimeError> {
        let row = sqlx::query_as::<_, UnitReleaseRow>(
            r#"
            SELECT id, version, version_code, package_name, package_path,
                   checksum_sha256, size_bytes, storage_provider, storage_bucket,
                   storage_object_key, storage_endpoint, storage_object_version_id
            FROM deployment_unit_releases
            WHERE id = ?1 AND unit_id = ?2 AND artifact_status = 'active'
            "#,
        )
        .bind(release_id)
        .bind(unit_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| Self::not_found("active deployment unit release not found"))?;
        if row.storage_provider == "local" && row.package_path.trim().is_empty() {
            return Err(Self::validation("local unit release package path is empty"));
        }
        if row.storage_provider == "aliyun_oss"
            && (row.storage_bucket.trim().is_empty() || row.storage_object_key.trim().is_empty())
        {
            return Err(Self::validation("OSS unit release location is incomplete"));
        }
        Ok(UnitReleaseSpec {
            id: row.id,
            version: row.version,
            version_code: row.version_code,
            package_name: row.package_name,
            package_path: PathBuf::from(row.package_path),
            checksum_sha256: row.checksum_sha256,
            size_bytes: row.size_bytes,
            storage_provider: row.storage_provider,
            storage_bucket: row.storage_bucket,
            storage_object_key: row.storage_object_key,
            storage_endpoint: row.storage_endpoint,
            storage_object_version_id: row.storage_object_version_id,
        })
    }

    async fn load_target_nodes(
        &self,
        environment_id: i64,
        target_node_ids: &[i64],
    ) -> Result<Vec<DeploymentTargetNode>, DeploymentRuntimeError> {
        if target_node_ids.is_empty() {
            return Err(Self::validation(
                "deployment environment has no target nodes",
            ));
        }
        let mut nodes = Vec::with_capacity(target_node_ids.len());
        for node_id in target_node_ids {
            let node = sqlx::query_as::<_, DeploymentTargetNode>(
                r#"
                SELECT nodes.id, nodes.node_key, nodes.name, nodes.node_type,
                       nodes.address, nodes.ssh_port, nodes.ssh_user,
                       credentials.private_key_path AS credential_private_key_path,
                       nodes.work_dir, nodes.status, nodes.docker_status
                FROM app_environment_targets targets
                JOIN nodes ON nodes.id = targets.node_id
                LEFT JOIN node_credentials credentials ON credentials.id = nodes.credential_id
                WHERE targets.environment_id = ?1 AND nodes.id = ?2
                "#,
            )
            .bind(environment_id)
            .bind(node_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| Self::validation("target node is not bound to environment"))?;
            if node.status != "online" || node.docker_status != "available" {
                return Err(Self::validation(&format!(
                    "target node {} is not ready for Docker deployment",
                    node.node_key
                )));
            }
            if node.node_type == "ssh"
                && (node.ssh_user.trim().is_empty()
                    || node
                        .credential_private_key_path
                        .as_deref()
                        .unwrap_or("")
                        .trim()
                        .is_empty())
            {
                return Err(Self::validation(&format!(
                    "SSH target node {} has no deploy credential",
                    node.node_key
                )));
            }
            nodes.push(node);
        }
        Ok(nodes)
    }

    fn validation(message: &str) -> DeploymentRuntimeError {
        DeploymentRuntimeError::Validation(message.to_owned())
    }

    fn not_found(message: &str) -> DeploymentRuntimeError {
        DeploymentRuntimeError::NotFound(message.to_owned())
    }
}

pub async fn prepare_unit_runtime(
    spec: &UnitExecutionSpec,
    staging_root: &Path,
) -> Result<PreparedUnitRuntime, DeploymentRuntimeError> {
    validate_environment_variables(&spec.environment_variables)?;
    let root = staging_root
        .join(spec.app_id.to_string())
        .join(spec.environment_id.to_string())
        .join(spec.unit_id.to_string());
    if fs::try_exists(&root)
        .await
        .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?
    {
        fs::remove_dir_all(&root)
            .await
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
    }
    fs::create_dir_all(&root)
        .await
        .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;

    let package_path = match &spec.release {
        Some(release) if release.storage_provider == "local" => {
            let source = release.package_path.clone();
            let expected_checksum = release.checksum_sha256.clone();
            let expected_size = release.size_bytes;
            let extract_root = root.clone();
            tokio::task::spawn_blocking(move || {
                verify_and_extract_package(
                    &source,
                    &extract_root,
                    &expected_checksum,
                    expected_size,
                )
            })
            .await
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))??;
            Some(release.package_path.clone())
        }
        Some(release) if release.storage_provider == "aliyun_oss" => {
            return Err(DeploymentRuntimeError::Validation(
                "OSS unit release must be downloaded before runtime preparation".to_owned(),
            ));
        }
        Some(_) => {
            return Err(DeploymentRuntimeError::Validation(
                "unsupported unit release storage provider".to_owned(),
            ));
        }
        None => None,
    };

    let compose_path = root.join("compose.yaml");
    fs::write(&compose_path, spec.unit.compose_content.as_bytes())
        .await
        .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
    let env_path = root.join(".env");
    fs::write(
        &env_path,
        render_environment_file(&spec.environment_variables),
    )
    .await
    .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
    let scripts_root = root.join(".easy-deploy").join("scripts");
    fs::create_dir_all(&scripts_root)
        .await
        .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
    let mut script_paths = BTreeMap::new();
    for (slot, content) in &spec.unit.scripts {
        let file_name = script_file_name(slot)?;
        let path = scripts_root.join(file_name);
        fs::write(&path, content.as_bytes())
            .await
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        script_paths.insert(slot.clone(), path);
    }
    Ok(PreparedUnitRuntime {
        root,
        compose_path,
        env_path,
        package_path,
        script_paths,
    })
}

pub async fn execute_prepared_unit_on_node(
    spec: &UnitExecutionSpec,
    prepared: &PreparedUnitRuntime,
    node: &DeploymentTargetNode,
    compose: &ComposeExecutor,
    ssh: &SshExecutor,
) -> Result<UnitNodeExecutionResult, DeploymentRuntimeError> {
    let work_dir = validated_target_work_dir(&spec.unit.work_dir)?;
    let mut outputs = Vec::new();
    let ssh_target = if node.node_type == "ssh" {
        Some(
            SshTarget::new(&node.ssh_user, &node.address, node.ssh_port)
                .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?
                .with_identity_file(
                    node.credential_private_key_path
                        .as_deref()
                        .map(str::trim)
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from),
                ),
        )
    } else if node.node_type == "local" {
        None
    } else {
        return Err(DeploymentRuntimeError::Validation(format!(
            "unsupported deployment node type {}",
            node.node_type
        )));
    };

    if spec.action != DeploymentAction::Stop {
        match &ssh_target {
            Some(target) => {
                sync_runtime_to_ssh(prepared, target, ssh, &work_dir, &mut outputs).await?;
            }
            None => copy_runtime_tree(&prepared.root, Path::new(&work_dir)).await?,
        }
    }
    if spec.action == DeploymentAction::Stop {
        let output = run_compose_action(
            DeploymentAction::Stop,
            prepared,
            ssh_target.as_ref(),
            compose,
            ssh,
            &work_dir,
        )
        .await?;
        return result_from_output(output, outputs, "Compose 服务已停止");
    }

    let env = execution_environment(spec);
    for slot in ["pre_deploy", "deploy", "post_deploy"] {
        if spec.unit.scripts.contains_key(slot) {
            let output = run_script(
                slot,
                prepared,
                ssh_target.as_ref(),
                compose,
                ssh,
                &work_dir,
                &env,
            )
            .await?;
            let success = output.success;
            let summary = command_summary(&output, &format!("脚本 {slot} 执行失败"));
            outputs.push(output);
            if !success {
                return Ok(UnitNodeExecutionResult {
                    success,
                    summary,
                    outputs,
                });
            }
        } else if slot == "deploy" {
            let output = run_compose_action(
                spec.action,
                prepared,
                ssh_target.as_ref(),
                compose,
                ssh,
                &work_dir,
            )
            .await?;
            let success = output.success;
            let summary = command_summary(&output, "Docker Compose 部署失败");
            outputs.push(output);
            if !success {
                return Ok(UnitNodeExecutionResult {
                    success,
                    summary,
                    outputs,
                });
            }
        }
    }

    let health = normalized_unit_health_check(&spec.unit)?;
    let health_result = run_node_health_check(
        &health,
        prepared,
        ssh_target.as_ref(),
        compose,
        ssh,
        &work_dir,
    )
    .await?;
    if let Some(output) = health_result.output {
        outputs.push(output);
    }
    if !health_result.healthy {
        return Ok(UnitNodeExecutionResult {
            success: false,
            summary: health_result.message,
            outputs,
        });
    }

    for slot in ["switch_traffic", "cleanup"] {
        if spec.unit.scripts.contains_key(slot) {
            let output = run_script(
                slot,
                prepared,
                ssh_target.as_ref(),
                compose,
                ssh,
                &work_dir,
                &env,
            )
            .await?;
            let success = output.success;
            let summary = command_summary(&output, &format!("脚本 {slot} 执行失败"));
            outputs.push(output);
            if !success {
                return Ok(UnitNodeExecutionResult {
                    success,
                    summary,
                    outputs,
                });
            }
        }
    }
    Ok(UnitNodeExecutionResult {
        success: true,
        summary: format!("节点 {} 部署成功：{}", node.node_key, health_result.message),
        outputs,
    })
}

struct NodeHealthResult {
    healthy: bool,
    message: String,
    output: Option<ComposeCommandOutput>,
}

async fn run_node_health_check(
    config: &crate::health::HealthCheckConfig,
    prepared: &PreparedUnitRuntime,
    target: Option<&SshTarget>,
    compose: &ComposeExecutor,
    ssh: &SshExecutor,
    work_dir: &str,
) -> Result<NodeHealthResult, DeploymentRuntimeError> {
    if config.kind == HealthCheckKind::ComposeRunning {
        let output = match target {
            Some(target) => {
                ssh.compose_ps_running(target, prepared.root.clone(), work_dir)
                    .await
            }
            None => compose.ps_running(PathBuf::from(work_dir)).await,
        }
        .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?;
        let healthy = output.success
            && output
                .output
                .lines()
                .any(|line| !line.trim().is_empty() && !line.trim().starts_with("NAME"));
        return Ok(NodeHealthResult {
            healthy,
            message: if healthy {
                "容器运行状态检查通过".to_owned()
            } else {
                command_summary(&output, "容器运行状态检查失败")
            },
            output: Some(output),
        });
    }
    if config.kind == HealthCheckKind::None {
        return Ok(NodeHealthResult {
            healthy: true,
            message: "未配置健康检查".to_owned(),
            output: None,
        });
    }
    let systemd =
        crate::deploy::SystemdExecutor::new(std::sync::Arc::new(UnsupportedHealthCommandRunner));
    let outcome = crate::health::run_health_check(config, compose, &systemd, prepared.root.clone())
        .await
        .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?;
    Ok(NodeHealthResult {
        healthy: outcome.healthy,
        message: outcome.message,
        output: None,
    })
}

struct UnsupportedHealthCommandRunner;

#[async_trait::async_trait]
impl crate::deploy::CommandRunner for UnsupportedHealthCommandRunner {
    async fn run(
        &self,
        _spec: crate::deploy::CommandSpec,
    ) -> Result<crate::deploy::CommandResult, crate::deploy::DeployError> {
        Err(crate::deploy::DeployError::InvalidInput(
            "systemd health checks are not supported for Compose units".to_owned(),
        ))
    }
}

async fn run_compose_action(
    action: DeploymentAction,
    prepared: &PreparedUnitRuntime,
    target: Option<&SshTarget>,
    compose: &ComposeExecutor,
    ssh: &SshExecutor,
    work_dir: &str,
) -> Result<ComposeCommandOutput, DeploymentRuntimeError> {
    let output = match (target, action) {
        (Some(target), DeploymentAction::Stop) => {
            ssh.compose_down(target, prepared.root.clone(), work_dir)
                .await
        }
        (Some(target), _) => {
            ssh.compose_up(target, prepared.root.clone(), work_dir)
                .await
        }
        (None, DeploymentAction::Stop) => compose.down(PathBuf::from(work_dir)).await,
        (None, _) => compose.up(PathBuf::from(work_dir)).await,
    };
    output.map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))
}

async fn run_script(
    slot: &str,
    prepared: &PreparedUnitRuntime,
    target: Option<&SshTarget>,
    compose: &ComposeExecutor,
    ssh: &SshExecutor,
    work_dir: &str,
    env: &[(String, String)],
) -> Result<ComposeCommandOutput, DeploymentRuntimeError> {
    let relative_path = format!(".easy-deploy/scripts/{}", script_file_name(slot)?);
    let output = match target {
        Some(target) => {
            ssh.run_script(target, prepared.root.clone(), work_dir, &relative_path, env)
                .await
        }
        None => {
            compose
                .run_script(PathBuf::from(work_dir), &relative_path, env)
                .await
        }
    };
    output.map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))
}

async fn sync_runtime_to_ssh(
    prepared: &PreparedUnitRuntime,
    target: &SshTarget,
    ssh: &SshExecutor,
    remote_root: &str,
    outputs: &mut Vec<ComposeCommandOutput>,
) -> Result<(), DeploymentRuntimeError> {
    let files = collect_runtime_files(&prepared.root)?;
    for (local_path, relative_path) in files {
        let remote_path = format!("{remote_root}/{}", relative_path.replace('\\', "/"));
        let remote_parent = remote_path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or(remote_root);
        let mkdir = ssh
            .mkdir_all(target, prepared.root.clone(), remote_parent)
            .await
            .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?;
        if !mkdir.success {
            return Err(DeploymentRuntimeError::Validation(command_summary(
                &mkdir,
                "SSH 创建部署目录失败",
            )));
        }
        outputs.push(mkdir);
        let copy = ssh
            .copy_file(target, prepared.root.clone(), local_path, &remote_path)
            .await
            .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?;
        if !copy.success {
            return Err(DeploymentRuntimeError::Validation(command_summary(
                &copy,
                "SSH 同步部署文件失败",
            )));
        }
        outputs.push(copy);
    }
    Ok(())
}

async fn copy_runtime_tree(
    source: &Path,
    destination: &Path,
) -> Result<(), DeploymentRuntimeError> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || copy_tree_sync(&source, &destination))
        .await
        .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?
}

fn copy_tree_sync(source: &Path, destination: &Path) -> Result<(), DeploymentRuntimeError> {
    std::fs::create_dir_all(destination)
        .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
    for (source_path, relative_path) in collect_runtime_files(source)? {
        let destination_path = destination.join(relative_path);
        if let Some(parent) = destination_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        }
        std::fs::copy(source_path, destination_path)
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
    }
    Ok(())
}

fn collect_runtime_files(root: &Path) -> Result<Vec<(PathBuf, String)>, DeploymentRuntimeError> {
    fn visit(
        root: &Path,
        current: &Path,
        files: &mut Vec<(PathBuf, String)>,
    ) -> Result<(), DeploymentRuntimeError> {
        for entry in std::fs::read_dir(current)
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?
        {
            let entry =
                entry.map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files)?;
            } else if path.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?
                    .to_string_lossy()
                    .to_string();
                files.push((path, relative));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(files)
}

fn execution_environment(spec: &UnitExecutionSpec) -> Vec<(String, String)> {
    let mut env = spec
        .environment_variables
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    env.extend([
        ("EASY_DEPLOY_APP_KEY".to_owned(), spec.app_key.clone()),
        (
            "EASY_DEPLOY_ENVIRONMENT".to_owned(),
            spec.environment_key.clone(),
        ),
        ("EASY_DEPLOY_UNIT_KEY".to_owned(), spec.unit_key.clone()),
        (
            "EASY_DEPLOY_VERSION".to_owned(),
            spec.release
                .as_ref()
                .map(|release| release.version.clone())
                .unwrap_or_default(),
        ),
    ]);
    env
}

fn normalized_unit_health_check(
    unit: &ConfigUnit,
) -> Result<crate::health::HealthCheckConfig, DeploymentRuntimeError> {
    let kind = unit
        .health_check
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("none");
    let endpoint = unit
        .health_check
        .get("endpoint")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let timeout = unit
        .health_check
        .get("timeout_secs")
        .and_then(|value| value.as_i64())
        .unwrap_or(5);
    let expected_status = unit
        .health_check
        .get("expected_status")
        .and_then(|value| value.as_i64())
        .unwrap_or(200);
    normalize_health_config(kind, endpoint, timeout, expected_status)
        .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))
}

fn validated_target_work_dir(work_dir: &str) -> Result<String, DeploymentRuntimeError> {
    let path = Path::new(work_dir.trim());
    if !path.is_absolute() || path.parent().is_none() || path == Path::new("/") {
        return Err(DeploymentRuntimeError::Validation(
            "deployment unit work_dir must be a non-root absolute path".to_owned(),
        ));
    }
    Ok(path
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_owned())
}

fn result_from_output(
    output: ComposeCommandOutput,
    mut outputs: Vec<ComposeCommandOutput>,
    success_message: &str,
) -> Result<UnitNodeExecutionResult, DeploymentRuntimeError> {
    let success = output.success;
    let summary = if success {
        success_message.to_owned()
    } else {
        command_summary(&output, "Docker Compose 命令失败")
    };
    outputs.push(output);
    Ok(UnitNodeExecutionResult {
        success,
        summary,
        outputs,
    })
}

fn command_summary(output: &ComposeCommandOutput, fallback: &str) -> String {
    output
        .output
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.chars().take(500).collect())
        .unwrap_or_else(|| fallback.to_owned())
}

fn verify_and_extract_package(
    package_path: &Path,
    destination: &Path,
    expected_checksum: &str,
    expected_size: i64,
) -> Result<(), DeploymentRuntimeError> {
    let metadata = std::fs::metadata(package_path).map_err(|error| {
        DeploymentRuntimeError::NotFound(format!("unit release package is unavailable: {error}"))
    })?;
    if expected_size > 0 && metadata.len() != expected_size as u64 {
        return Err(DeploymentRuntimeError::Validation(format!(
            "unit release package size mismatch: expected {expected_size}, got {}",
            metadata.len()
        )));
    }
    let mut file = File::open(package_path)
        .map_err(|error| DeploymentRuntimeError::NotFound(error.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual_checksum = format!("{:x}", hasher.finalize());
    if !expected_checksum.eq_ignore_ascii_case(&actual_checksum) {
        return Err(DeploymentRuntimeError::Validation(
            "unit release package checksum mismatch".to_owned(),
        ));
    }
    let file = File::open(package_path)
        .map_err(|error| DeploymentRuntimeError::NotFound(error.to_string()))?;
    Archive::new(GzDecoder::new(file))
        .unpack(destination)
        .map_err(|error| {
            DeploymentRuntimeError::Validation(format!(
                "unit release package cannot be extracted safely: {error}"
            ))
        })
}

fn validate_environment_variables(
    variables: &BTreeMap<String, String>,
) -> Result<(), DeploymentRuntimeError> {
    for (name, value) in variables {
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            && !name.as_bytes()[0].is_ascii_digit();
        if !valid_name || value.contains(['\r', '\n']) {
            return Err(DeploymentRuntimeError::Validation(format!(
                "invalid deployment environment variable {name}"
            )));
        }
    }
    Ok(())
}

fn render_environment_file(variables: &BTreeMap<String, String>) -> Vec<u8> {
    let mut output = String::new();
    for (name, value) in variables {
        output.push_str(name);
        output.push('=');
        output.push_str(value);
        output.push('\n');
    }
    output.into_bytes()
}

fn script_file_name(slot: &str) -> Result<&'static str, DeploymentRuntimeError> {
    match slot {
        "pre_deploy" => Ok("pre-deploy.sh"),
        "deploy" => Ok("deploy.sh"),
        "post_deploy" => Ok("post-deploy.sh"),
        "switch_traffic" => Ok("switch-traffic.sh"),
        "cleanup" => Ok("cleanup.sh"),
        _ => Err(DeploymentRuntimeError::Validation(format!(
            "unsupported deployment script slot {slot}"
        ))),
    }
}

fn environment_variables(
    secrets: &BTreeMap<String, String>,
    environment_key: &str,
    unit_key: &str,
) -> BTreeMap<String, String> {
    let prefix = format!("{environment_key}.{unit_key}.");
    secrets
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix(&prefix)
                .map(|name| (name.to_owned(), value.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use flate2::{Compression, write::GzEncoder};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[derive(Default)]
    struct RecordingCommandRunner {
        commands: Mutex<Vec<crate::deploy::CommandSpec>>,
    }

    #[async_trait]
    impl crate::deploy::CommandRunner for RecordingCommandRunner {
        async fn run(
            &self,
            spec: crate::deploy::CommandSpec,
        ) -> Result<crate::deploy::CommandResult, crate::deploy::DeployError> {
            self.commands.lock().expect("command lock").push(spec);
            Ok(crate::deploy::CommandResult {
                status_code: Some(0),
                stdout: "ok".to_owned(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn extracts_only_selected_environment_and_unit_secrets() {
        let values = BTreeMap::from([
            ("production.api.DATABASE_URL".to_owned(), "prod".to_owned()),
            ("test.api.DATABASE_URL".to_owned(), "test".to_owned()),
            ("production.web.TOKEN".to_owned(), "web".to_owned()),
        ]);
        assert_eq!(
            environment_variables(&values, "production", "api"),
            BTreeMap::from([("DATABASE_URL".to_owned(), "prod".to_owned())])
        );
    }

    #[tokio::test]
    async fn prepares_verified_package_compose_environment_and_scripts() {
        let temp = tempdir().expect("create temp dir");
        let package_path = temp.path().join("api.tar.gz");
        let package = File::create(&package_path).expect("create package");
        let encoder = GzEncoder::new(package, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let payload = b"release-content";
        let mut header = tar::Header::new_gnu();
        header.set_path("release.txt").expect("set path");
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append(&header, payload.as_slice())
            .expect("append payload");
        let encoder = archive.into_inner().expect("finish archive");
        encoder.finish().expect("finish gzip");
        let bytes = std::fs::read(&package_path).expect("read package");
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        let target_work_dir = temp.path().join("target");
        let spec = UnitExecutionSpec {
            app_id: 1,
            app_key: "orders".to_owned(),
            environment_id: 2,
            environment_key: "production".to_owned(),
            config_revision_id: 3,
            config_hash: "config-hash".to_owned(),
            unit_id: 4,
            unit_key: "api".to_owned(),
            unit: ConfigUnit {
                key: "api".to_owned(),
                name: "API".to_owned(),
                required: true,
                status: "active".to_owned(),
                work_dir: target_work_dir.to_string_lossy().to_string(),
                compose_content: "services:\n  api:\n    image: example/api".to_owned(),
                scripts: BTreeMap::from([
                    ("pre_deploy".to_owned(), "echo pre".to_owned()),
                    ("deploy".to_owned(), "docker compose up -d".to_owned()),
                    ("post_deploy".to_owned(), "echo post".to_owned()),
                    ("switch_traffic".to_owned(), "echo switch".to_owned()),
                    ("cleanup".to_owned(), "echo cleanup".to_owned()),
                ]),
                health_check: serde_json::json!({}),
            },
            action: DeploymentAction::Deploy,
            release: Some(UnitReleaseSpec {
                id: 5,
                version: "1.0.0".to_owned(),
                version_code: 100,
                package_name: "api.tar.gz".to_owned(),
                package_path: package_path.clone(),
                checksum_sha256: checksum,
                size_bytes: bytes.len() as i64,
                storage_provider: "local".to_owned(),
                storage_bucket: String::new(),
                storage_object_key: String::new(),
                storage_endpoint: String::new(),
                storage_object_version_id: String::new(),
            }),
            target_nodes: Vec::new(),
            environment_variables: BTreeMap::from([("APP_SECRET".to_owned(), "secret".to_owned())]),
        };

        let prepared = prepare_unit_runtime(&spec, &temp.path().join("staging"))
            .await
            .expect("prepare runtime");
        assert_eq!(
            fs::read_to_string(prepared.root.join("release.txt"))
                .await
                .expect("read extracted payload"),
            "release-content"
        );
        assert_eq!(
            fs::read_to_string(&prepared.env_path)
                .await
                .expect("read env"),
            "APP_SECRET=secret\n"
        );
        assert!(prepared.compose_path.is_file());
        assert!(prepared.script_paths["deploy"].is_file());

        let runner = Arc::new(RecordingCommandRunner::default());
        let compose = ComposeExecutor::new(runner.clone());
        let ssh = SshExecutor::new(runner.clone());
        let node = DeploymentTargetNode {
            id: 1,
            node_key: "local".to_owned(),
            name: "Local".to_owned(),
            node_type: "local".to_owned(),
            address: "127.0.0.1".to_owned(),
            ssh_port: 22,
            ssh_user: String::new(),
            credential_private_key_path: None,
            work_dir: target_work_dir.to_string_lossy().to_string(),
            status: "online".to_owned(),
            docker_status: "available".to_owned(),
        };
        let result = execute_prepared_unit_on_node(&spec, &prepared, &node, &compose, &ssh)
            .await
            .expect("execute prepared runtime");
        assert!(result.success);
        assert_eq!(result.outputs.len(), 5);
        assert!(target_work_dir.join("compose.yaml").is_file());
        assert!(target_work_dir.join("release.txt").is_file());
        {
            let commands = runner.commands.lock().expect("command lock");
            assert_eq!(commands.len(), 5);
            assert!(commands.iter().all(|command| command.program == "env"));
        }

        let mut corrupted = spec;
        corrupted.release.as_mut().expect("release").checksum_sha256 = "0".repeat(64);
        assert!(matches!(
            prepare_unit_runtime(&corrupted, &temp.path().join("bad-staging")).await,
            Err(DeploymentRuntimeError::Validation(message)) if message.contains("checksum")
        ));
    }

    #[test]
    fn rejects_environment_file_injection_and_unknown_script_slots() {
        assert!(
            validate_environment_variables(&BTreeMap::from([(
                "TOKEN".to_owned(),
                "value\nINJECTED=true".to_owned()
            )]))
            .is_err()
        );
        assert!(script_file_name("unknown").is_err());
    }
}
