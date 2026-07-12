use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use tar::Archive;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use crate::{
    application_config::{ApplicationConfigService, ConfigUnit},
    deploy::{
        CommandOutputSink, CommandOutputStream, ComposeCommandOutput, ComposeExecutor, DeployError,
        DynCommandOutputSink, SshExecutor, SshTarget,
    },
    deployment_orchestrator::{
        DeploymentAction, DeploymentUnitExecutor, UnitExecutionContext, UnitExecutionOutcome,
    },
    deployment_retention::{DeploymentLogService, StreamingRedactor, redact_log_text},
    health::{HealthCheckKind, normalize_health_config},
    platform::PlatformConfigService,
};

#[derive(Clone)]
pub struct DeploymentRuntimeService {
    db: SqlitePool,
    configs: ApplicationConfigService,
}

#[derive(Clone)]
pub struct ComposeDeploymentUnitExecutor {
    runtime: DeploymentRuntimeService,
    compose: ComposeExecutor,
    ssh: SshExecutor,
    staging_root: PathBuf,
    logs: DeploymentLogService,
    platform: PlatformConfigService,
    http: reqwest::Client,
}

struct DeploymentStepOutputSink {
    sender: mpsc::Sender<DeploymentLogWriterMessage>,
    dropped_bytes: Arc<AtomicU64>,
    redactor: Mutex<StreamingRedactor>,
}

enum DeploymentLogWriterMessage {
    Chunk(Vec<u8>),
    Barrier(oneshot::Sender<Result<(), String>>),
}

const DEPLOYMENT_LOG_FLUSH_BYTES: usize = 1024 * 1024;
const DEPLOYMENT_LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
const DEPLOYMENT_LOG_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);
const DEPLOYMENT_LOG_TRUNCATED_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
const DEPLOYMENT_LOG_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(2);
const DEPLOYMENT_LOG_WRITER_CAPACITY: usize = 256;
const DEPLOYMENT_LOG_WRITER_CHUNK_BYTES: usize = 16 * 1024;

impl DeploymentStepOutputSink {
    fn new(logs: DeploymentLogService, task_id: i64, step_id: i64, secrets: Vec<String>) -> Self {
        let (sender, receiver) = mpsc::channel(DEPLOYMENT_LOG_WRITER_CAPACITY);
        let dropped_bytes = Arc::new(AtomicU64::new(0));
        tokio::spawn(run_deployment_log_writer(
            logs,
            task_id,
            step_id,
            receiver,
            dropped_bytes.clone(),
        ));
        Self {
            sender,
            dropped_bytes,
            redactor: Mutex::new(StreamingRedactor::new(secrets)),
        }
    }

    fn enqueue(&self, chunk: &[u8]) -> Result<(), DeployError> {
        for chunk in chunk.chunks(DEPLOYMENT_LOG_WRITER_CHUNK_BYTES) {
            match self
                .sender
                .try_send(DeploymentLogWriterMessage::Chunk(chunk.to_vec()))
            {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.dropped_bytes
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(DeployError::Command(
                        "部署步骤日志 writer 已停止".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn finish_redaction(&self) -> Result<(), DeployError> {
        let final_bytes = self
            .redactor
            .lock()
            .map_err(|_| DeployError::Command("部署日志脱敏器状态损坏".to_owned()))?
            .finish();
        self.enqueue(&final_bytes)?;
        self.flush().await
    }
}

#[async_trait::async_trait]
impl CommandOutputSink for DeploymentStepOutputSink {
    async fn write(&self, _stream: CommandOutputStream, chunk: &[u8]) -> Result<(), DeployError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let redacted = self
            .redactor
            .lock()
            .map_err(|_| DeployError::Command("部署日志脱敏器状态损坏".to_owned()))?
            .push(chunk);
        self.enqueue(&redacted)
    }

    async fn flush(&self) -> Result<(), DeployError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(DeploymentLogWriterMessage::Barrier(sender))
            .await
            .map_err(|_| DeployError::Command("部署步骤日志 writer 已停止".to_owned()))?;
        receiver
            .await
            .map_err(|_| DeployError::Command("部署步骤日志 writer 未完成刷新".to_owned()))?
            .map_err(|error| DeployError::Command(format!("写入部署步骤日志失败: {error}")))
    }
}

async fn run_deployment_log_writer(
    logs: DeploymentLogService,
    task_id: i64,
    step_id: i64,
    mut receiver: mpsc::Receiver<DeploymentLogWriterMessage>,
    dropped_bytes: Arc<AtomicU64>,
) {
    let mut pending = Vec::new();
    let mut dirty = false;
    let mut last_checkpoint = Instant::now();
    let mut checkpoint_interval = DEPLOYMENT_LOG_CHECKPOINT_INTERVAL;
    let mut tick = tokio::time::interval(DEPLOYMENT_LOG_FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            message = receiver.recv() => {
                match message {
                    Some(DeploymentLogWriterMessage::Chunk(chunk)) => {
                        pending.extend_from_slice(&chunk);
                        if pending.len() >= DEPLOYMENT_LOG_FLUSH_BYTES {
                            match flush_deployment_log_buffer(
                                &logs,
                                task_id,
                                step_id,
                                &mut pending,
                                &dropped_bytes,
                            ).await {
                                Ok(changed) => dirty |= changed,
                                Err(error) => tracing::warn!(task_id, step_id, %error, "failed to buffer deployment output"),
                            }
                        }
                    }
                    Some(DeploymentLogWriterMessage::Barrier(reply)) => {
                        let result = flush_deployment_log_buffer(
                            &logs,
                            task_id,
                            step_id,
                            &mut pending,
                            &dropped_bytes,
                        ).await;
                        if result.as_ref().is_ok_and(|changed| *changed) {
                            dirty = true;
                        }
                        let _ = reply.send(result.map(|_| ()));
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                match flush_deployment_log_buffer(
                    &logs,
                    task_id,
                    step_id,
                    &mut pending,
                    &dropped_bytes,
                ).await {
                    Ok(changed) => dirty |= changed,
                    Err(error) => tracing::warn!(task_id, step_id, %error, "failed to buffer deployment output"),
                }
                if dirty && last_checkpoint.elapsed() >= checkpoint_interval {
                    match tokio::time::timeout(
                        DEPLOYMENT_LOG_CHECKPOINT_TIMEOUT,
                        logs.checkpoint(task_id, step_id),
                    ).await {
                        Ok(Ok(snapshot)) => {
                            dirty = false;
                            last_checkpoint = Instant::now();
                            checkpoint_interval = if snapshot.truncated {
                                DEPLOYMENT_LOG_TRUNCATED_CHECKPOINT_INTERVAL
                            } else {
                                DEPLOYMENT_LOG_CHECKPOINT_INTERVAL
                            };
                        }
                        Ok(Err(error)) => tracing::warn!(task_id, step_id, %error, "failed to checkpoint deployment output"),
                        Err(_) => tracing::warn!(task_id, step_id, "timed out checkpointing deployment output"),
                    }
                }
            }
        }
    }
}

async fn flush_deployment_log_buffer(
    logs: &DeploymentLogService,
    task_id: i64,
    step_id: i64,
    pending: &mut Vec<u8>,
    dropped_bytes: &AtomicU64,
) -> Result<bool, String> {
    let mut changed = false;
    if !pending.is_empty() {
        logs.append_buffered(task_id, step_id, &[], pending)
            .await
            .map_err(|error| error.to_string())?;
        pending.clear();
        changed = true;
    }
    let dropped = dropped_bytes.swap(0, Ordering::AcqRel);
    if dropped > 0 {
        if let Err(error) = logs.record_dropped(task_id, step_id, &[], dropped).await {
            dropped_bytes.fetch_add(dropped, Ordering::Release);
            return Err(error.to_string());
        }
        changed = true;
    }
    Ok(changed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentRuntimeError {
    Validation(String),
    NotFound(String),
    Config(String),
    Database(String),
    Canceled(String),
}

impl std::fmt::Display for DeploymentRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::NotFound(message)
            | Self::Config(message)
            | Self::Database(message)
            | Self::Canceled(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DeploymentRuntimeError {}

impl From<sqlx::Error> for DeploymentRuntimeError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<DeployError> for DeploymentRuntimeError {
    fn from(error: DeployError) -> Self {
        match error {
            DeployError::Canceled(message) => Self::Canceled(message),
            DeployError::InvalidInput(message) | DeployError::Command(message) => {
                Self::Validation(message)
            }
        }
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

impl ComposeDeploymentUnitExecutor {
    pub fn new(
        runtime: DeploymentRuntimeService,
        compose: ComposeExecutor,
        ssh: SshExecutor,
        staging_root: PathBuf,
        logs: DeploymentLogService,
        platform: PlatformConfigService,
    ) -> Self {
        Self {
            runtime,
            compose,
            ssh,
            staging_root,
            logs,
            platform,
            http: reqwest::Client::new(),
        }
    }

    async fn execute_inner(
        &self,
        context: &UnitExecutionContext,
    ) -> Result<String, DeploymentRuntimeError> {
        let mut spec = self.runtime.load_unit_spec(context).await?;
        let result = self.execute_spec(context, &mut spec).await;
        let result = merge_execution_and_log_finish(
            result,
            self.logs.finish(context.task_id, context.step_id).await,
        );
        let cleanup = cleanup_execution_staging(&self.staging_root, context, &spec).await;
        merge_execution_and_cleanup(result, cleanup)
    }

    async fn execute_spec(
        &self,
        context: &UnitExecutionContext,
        spec: &mut UnitExecutionSpec,
    ) -> Result<String, DeploymentRuntimeError> {
        ensure_not_canceled(context)?;
        self.materialize_oss_release(context, spec).await?;
        ensure_not_canceled(context)?;
        let prepared = prepare_unit_runtime(spec, &self.staging_root).await?;
        ensure_not_canceled(context)?;
        let secrets = spec
            .environment_variables
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let step_output_sink = Arc::new(DeploymentStepOutputSink::new(
            self.logs.clone(),
            context.task_id,
            context.step_id,
            secrets.clone(),
        ));
        let output_sink: DynCommandOutputSink = step_output_sink.clone();
        let compose = self
            .compose
            .with_cancellation(context.cancellation.clone())
            .with_output_sink(output_sink.clone());
        let ssh = self
            .ssh
            .with_cancellation(context.cancellation.clone())
            .with_output_sink(output_sink);
        let result = async {
            let mut summaries = Vec::new();
            for node in &spec.target_nodes {
                ensure_not_canceled(context)?;
                let result = execute_prepared_unit_on_node(spec, &prepared, node, &compose, &ssh)
                    .await
                    .map_err(|error| redact_runtime_error(error, &secrets))?;
                let result = redact_node_execution_result(result, &secrets);
                summaries.push(result.summary.clone());
                if !result.success {
                    return Err(DeploymentRuntimeError::Validation(format!(
                        "节点 {} 执行失败：{}",
                        node.node_key, result.summary
                    )));
                }
            }
            Ok(summaries.join("；"))
        }
        .await;
        let finish = step_output_sink
            .finish_redaction()
            .await
            .map_err(DeploymentRuntimeError::from);
        merge_execution_and_output_finish(result, finish)
    }

    async fn materialize_oss_release(
        &self,
        context: &UnitExecutionContext,
        spec: &mut UnitExecutionSpec,
    ) -> Result<(), DeploymentRuntimeError> {
        ensure_not_canceled(context)?;
        let Some(release) = spec.release.as_mut() else {
            return Ok(());
        };
        if release.storage_provider != "aliyun_oss" {
            return Ok(());
        }
        let platform = self
            .platform
            .config()
            .await
            .map_err(|error| DeploymentRuntimeError::Config(error.to_string()))?;
        if platform.artifact_storage.provider != "aliyun_oss" {
            return Err(DeploymentRuntimeError::Config(
                "unit release uses OSS but platform OSS storage is disabled".to_owned(),
            ));
        }
        let oss = &platform.artifact_storage.aliyun_oss;
        if oss.bucket != release.storage_bucket
            || oss.endpoint.trim_end_matches('/') != release.storage_endpoint.trim_end_matches('/')
        {
            return Err(DeploymentRuntimeError::Config(
                "unit release OSS location does not match current platform storage".to_owned(),
            ));
        }
        let download = oss
            .presign_download_version(
                &release.storage_object_key,
                (!release.storage_object_version_id.trim().is_empty())
                    .then_some(release.storage_object_version_id.as_str()),
            )
            .map_err(|error| DeploymentRuntimeError::Config(error.to_string()))?;
        let package_name = safe_package_name(&release.package_name)?;
        let destination = self
            .staging_root
            .join("downloads")
            .join(context.deployment_run_id.to_string())
            .join(context.item.unit_id.to_string())
            .join(package_name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        }
        let mut response = self.http.get(download.url).send().await.map_err(|error| {
            DeploymentRuntimeError::NotFound(format!("download OSS unit release failed: {error}"))
        })?;
        if !response.status().is_success() {
            return Err(DeploymentRuntimeError::NotFound(format!(
                "download OSS unit release returned HTTP {}",
                response.status()
            )));
        }
        let mut file = fs::File::create(&destination)
            .await
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        let mut hasher = Sha256::new();
        let mut received = 0_u64;
        loop {
            let chunk = tokio::select! {
                _ = context.cancellation.cancelled() => {
                    let _ = fs::remove_file(&destination).await;
                    return Err(DeploymentRuntimeError::Canceled(
                        "部署已取消，停止下载 OSS 单元发布包".to_owned(),
                    ));
                }
                chunk = response.chunk() => chunk.map_err(|error| {
                    DeploymentRuntimeError::NotFound(format!("read OSS unit release failed: {error}"))
                })?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            received = received.saturating_add(chunk.len() as u64);
            if release.size_bytes > 0 && received > release.size_bytes as u64 {
                let _ = fs::remove_file(&destination).await;
                return Err(DeploymentRuntimeError::Validation(
                    "OSS unit release is larger than registered size".to_owned(),
                ));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        }
        file.flush()
            .await
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?;
        let checksum = format!("{:x}", hasher.finalize());
        if (release.size_bytes > 0 && received != release.size_bytes as u64)
            || !release.checksum_sha256.eq_ignore_ascii_case(&checksum)
        {
            let _ = fs::remove_file(&destination).await;
            return Err(DeploymentRuntimeError::Validation(
                "downloaded OSS unit release integrity check failed".to_owned(),
            ));
        }
        release.package_path = destination;
        release.storage_provider = "local".to_owned();
        Ok(())
    }
}

fn redact_runtime_error(
    error: DeploymentRuntimeError,
    secrets: &[String],
) -> DeploymentRuntimeError {
    let message = redact_log_text(secrets, &error.to_string());
    match error {
        DeploymentRuntimeError::Validation(_) => DeploymentRuntimeError::Validation(message),
        DeploymentRuntimeError::NotFound(_) => DeploymentRuntimeError::NotFound(message),
        DeploymentRuntimeError::Config(_) => DeploymentRuntimeError::Config(message),
        DeploymentRuntimeError::Database(_) => DeploymentRuntimeError::Database(message),
        DeploymentRuntimeError::Canceled(_) => DeploymentRuntimeError::Canceled(message),
    }
}

fn redact_node_execution_result(
    mut result: UnitNodeExecutionResult,
    secrets: &[String],
) -> UnitNodeExecutionResult {
    result.summary = redact_log_text(secrets, &result.summary);
    result
}

fn merge_execution_and_log_finish(
    result: Result<String, DeploymentRuntimeError>,
    finish: Result<
        crate::deployment_retention::BoundedLogSnapshot,
        crate::deployment_retention::DeploymentRetentionError,
    >,
) -> Result<String, DeploymentRuntimeError> {
    match (result, finish) {
        (Ok(summary), Ok(_)) => Ok(summary),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(DeploymentRuntimeError::Database(format!(
            "部署完成，但步骤日志收口失败：{error}"
        ))),
        (Err(DeploymentRuntimeError::Canceled(message)), Err(log_error)) => Err(
            DeploymentRuntimeError::Canceled(format!("{message}；步骤日志收口失败：{log_error}")),
        ),
        (Err(error), Err(log_error)) => Err(DeploymentRuntimeError::Database(format!(
            "部署失败：{error}；步骤日志收口失败：{log_error}"
        ))),
    }
}

fn merge_execution_and_output_finish(
    result: Result<String, DeploymentRuntimeError>,
    finish: Result<(), DeploymentRuntimeError>,
) -> Result<String, DeploymentRuntimeError> {
    match (result, finish) {
        (Ok(summary), Ok(())) => Ok(summary),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(DeploymentRuntimeError::Canceled(message)), Err(output_error)) => {
            Err(DeploymentRuntimeError::Canceled(format!(
                "{message}；部署日志脱敏收口失败：{output_error}"
            )))
        }
        (Err(error), Err(output_error)) => Err(DeploymentRuntimeError::Database(format!(
            "部署失败：{error}；部署日志脱敏收口失败：{output_error}"
        ))),
    }
}

fn merge_execution_and_cleanup(
    result: Result<String, DeploymentRuntimeError>,
    cleanup: Result<(), DeploymentRuntimeError>,
) -> Result<String, DeploymentRuntimeError> {
    match (result, cleanup) {
        (Ok(summary), Ok(())) => Ok(summary),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(DeploymentRuntimeError::Canceled(message)), Err(cleanup_error)) => {
            Err(DeploymentRuntimeError::Canceled(format!(
                "{message}；staging 清理失败：{cleanup_error}"
            )))
        }
        (Err(error), Err(cleanup_error)) => Err(DeploymentRuntimeError::Database(format!(
            "deployment failed: {error}; staging cleanup failed: {cleanup_error}"
        ))),
    }
}

async fn cleanup_execution_staging(
    staging_root: &Path,
    context: &UnitExecutionContext,
    spec: &UnitExecutionSpec,
) -> Result<(), DeploymentRuntimeError> {
    let unit_root = staging_root
        .join(spec.app_id.to_string())
        .join(spec.environment_id.to_string())
        .join(spec.unit_id.to_string());
    let download_root = staging_root
        .join("downloads")
        .join(context.deployment_run_id.to_string())
        .join(context.item.unit_id.to_string());
    for path in [unit_root, download_root] {
        if fs::try_exists(&path)
            .await
            .map_err(|error| DeploymentRuntimeError::Database(error.to_string()))?
        {
            fs::remove_dir_all(&path).await.map_err(|error| {
                DeploymentRuntimeError::Database(format!(
                    "remove deployment staging {} failed: {error}",
                    path.to_string_lossy()
                ))
            })?;
        }
    }
    Ok(())
}

fn ensure_not_canceled(context: &UnitExecutionContext) -> Result<(), DeploymentRuntimeError> {
    if context.cancellation.is_cancelled() {
        Err(DeploymentRuntimeError::Canceled(
            "部署取消请求已生效，停止执行当前单元".to_owned(),
        ))
    } else {
        Ok(())
    }
}

#[async_trait::async_trait]
impl DeploymentUnitExecutor for ComposeDeploymentUnitExecutor {
    async fn execute(&self, context: UnitExecutionContext) -> UnitExecutionOutcome {
        match self.execute_inner(&context).await {
            Ok(summary) => UnitExecutionOutcome::Success { summary },
            Err(error) => {
                let _ = self
                    .logs
                    .append(
                        context.task_id,
                        context.step_id,
                        &[],
                        format!("部署执行失败：{error}\n").as_bytes(),
                    )
                    .await;
                let _ = self.logs.finish(context.task_id, context.step_id).await;
                match error {
                    DeploymentRuntimeError::Canceled(summary) => {
                        UnitExecutionOutcome::CanceledUnknown { summary }
                    }
                    error => UnitExecutionOutcome::Failed {
                        failure_kind: match &error {
                            DeploymentRuntimeError::Validation(_) => "validation",
                            DeploymentRuntimeError::NotFound(_) => "resource_not_found",
                            DeploymentRuntimeError::Config(_) => "config_error",
                            DeploymentRuntimeError::Database(_) => "database_error",
                            DeploymentRuntimeError::Canceled(_) => unreachable!(),
                        }
                        .to_owned(),
                        summary: error.to_string(),
                        exit_code: None,
                    },
                }
            }
        }
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
                sync_runtime_to_ssh(prepared, target, ssh, &work_dir).await?;
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
        return Ok(result_from_output(output, "Compose 服务已停止"));
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
            if !success {
                return Ok(UnitNodeExecutionResult { success, summary });
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
            if !success {
                return Ok(UnitNodeExecutionResult { success, summary });
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
    if !health_result.healthy {
        return Ok(UnitNodeExecutionResult {
            success: false,
            summary: health_result.message,
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
            if !success {
                return Ok(UnitNodeExecutionResult { success, summary });
            }
        }
    }
    Ok(UnitNodeExecutionResult {
        success: true,
        summary: format!("节点 {} 部署成功：{}", node.node_key, health_result.message),
    })
}

struct NodeHealthResult {
    healthy: bool,
    message: String,
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
        .map_err(DeploymentRuntimeError::from)?;
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
        });
    }
    if config.kind == HealthCheckKind::None {
        return Ok(NodeHealthResult {
            healthy: true,
            message: "未配置健康检查".to_owned(),
        });
    }
    if let Some(target) = target {
        let output = match config.kind {
            HealthCheckKind::Http => {
                ssh.http_health_check(
                    target,
                    prepared.root.clone(),
                    &config.endpoint,
                    config.timeout_secs,
                )
                .await
            }
            HealthCheckKind::Tcp => {
                ssh.tcp_health_check(
                    target,
                    prepared.root.clone(),
                    &config.endpoint,
                    config.timeout_secs,
                )
                .await
            }
            HealthCheckKind::SystemdActive => {
                return Err(DeploymentRuntimeError::Validation(
                    "systemd health checks are not supported for Compose deployment units"
                        .to_owned(),
                ));
            }
            HealthCheckKind::None | HealthCheckKind::ComposeRunning => unreachable!(),
        }
        .map_err(DeploymentRuntimeError::from)?;
        let healthy = match config.kind {
            HealthCheckKind::Http => {
                output.success && output.output.trim() == config.expected_status.to_string()
            }
            HealthCheckKind::Tcp => output.success,
            _ => false,
        };
        let message = if healthy {
            format!("{} 健康检查通过", config.kind.label())
        } else {
            command_summary(&output, &format!("{} 健康检查失败", config.kind.label()))
        };
        return Ok(NodeHealthResult { healthy, message });
    }
    if config.kind == HealthCheckKind::SystemdActive {
        return Err(DeploymentRuntimeError::Validation(
            "systemd health checks are not supported for Compose deployment units".to_owned(),
        ));
    }
    let systemd =
        crate::deploy::SystemdExecutor::new(std::sync::Arc::new(UnsupportedHealthCommandRunner));
    let outcome = crate::health::run_health_check(config, compose, &systemd, prepared.root.clone())
        .await
        .map_err(|error| DeploymentRuntimeError::Validation(error.to_string()))?;
    Ok(NodeHealthResult {
        healthy: outcome.healthy,
        message: outcome.message,
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
    output.map_err(DeploymentRuntimeError::from)
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
    output.map_err(DeploymentRuntimeError::from)
}

async fn sync_runtime_to_ssh(
    prepared: &PreparedUnitRuntime,
    target: &SshTarget,
    ssh: &SshExecutor,
    remote_root: &str,
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
            .map_err(DeploymentRuntimeError::from)?;
        if !mkdir.success {
            return Err(DeploymentRuntimeError::Validation(command_summary(
                &mkdir,
                "SSH 创建部署目录失败",
            )));
        }
        let copy = ssh
            .copy_file(target, prepared.root.clone(), local_path, &remote_path)
            .await
            .map_err(DeploymentRuntimeError::from)?;
        if !copy.success {
            return Err(DeploymentRuntimeError::Validation(command_summary(
                &copy,
                "SSH 同步部署文件失败",
            )));
        }
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
    success_message: &str,
) -> UnitNodeExecutionResult {
    let success = output.success;
    let summary = if success {
        success_message.to_owned()
    } else {
        command_summary(&output, "Docker Compose 命令失败")
    };
    UnitNodeExecutionResult { success, summary }
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

fn safe_package_name(name: &str) -> Result<&str, DeploymentRuntimeError> {
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || Path::new(name).file_name().and_then(|value| value.to_str()) != Some(name)
    {
        return Err(DeploymentRuntimeError::Validation(
            "unit release package name is unsafe".to_owned(),
        ));
    }
    Ok(name)
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
    use sqlx::sqlite::SqlitePoolOptions;
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
                stdout: "200".to_owned(),
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

    #[test]
    fn deployment_result_and_error_summaries_are_redacted() {
        let secrets = vec!["very-sensitive-value".to_owned()];
        let result = redact_node_execution_result(
            UnitNodeExecutionResult {
                success: false,
                summary: "deploy failed with very-sensitive-value".to_owned(),
            },
            &secrets,
        );
        assert!(!result.summary.contains("very-sensitive-value"));

        let error = redact_runtime_error(
            DeploymentRuntimeError::Validation(
                "command env TOKEN=very-sensitive-value failed".to_owned(),
            ),
            &secrets,
        );
        assert!(!error.to_string().contains("very-sensitive-value"));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn deployment_output_sink_redacts_secrets_across_chunks() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect database");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("run migrations");
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, status, created_by) VALUES ('release.deploy', 'stream logs', 'running', 'admin')",
        )
        .execute(&db)
        .await
        .expect("create task")
        .last_insert_rowid();
        let step_id = sqlx::query(
            "INSERT INTO operation_task_steps(task_id, step_no, step_key, title, status) VALUES (?1, 1, 'unit-api', 'Deploy API', 'running')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("create step")
        .last_insert_rowid();
        let logs = DeploymentLogService::with_limits(db, 1024, 1024, 2048);
        let sink = DeploymentStepOutputSink::new(
            logs.clone(),
            task_id,
            step_id,
            vec!["very-sensitive-value".to_owned()],
        );

        sink.write(CommandOutputStream::Stdout, b"token=very-sensi")
            .await
            .expect("write first chunk");
        sink.write(
            CommandOutputStream::Stdout,
            b"tive-value\npassword=hunter2\nready\n",
        )
        .await
        .expect("write second chunk");
        sink.finish_redaction().await.expect("finish output sink");
        let snapshot = logs.finish(task_id, step_id).await.expect("finish log");
        let stored =
            String::from_utf8_lossy(&[snapshot.head.as_slice(), snapshot.tail.as_slice()].concat())
                .into_owned();
        assert!(!stored.contains("very-sensitive-value"));
        assert!(!stored.contains("hunter2"));
        assert!(stored.contains("[REDACTED]"));
        assert!(stored.contains("ready"));
    }

    #[tokio::test]
    async fn deployment_output_sink_drops_with_accounting_instead_of_backpressuring() {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect database");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("run migrations");
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, status, created_by) VALUES ('release.deploy', 'backpressure', 'running', 'admin')",
        )
        .execute(&db)
        .await
        .expect("create task")
        .last_insert_rowid();
        let step_id = sqlx::query(
            "INSERT INTO operation_task_steps(task_id, step_no, step_key, title, status) VALUES (?1, 1, 'unit-api', 'Deploy API', 'running')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("create step")
        .last_insert_rowid();
        let logs =
            DeploymentLogService::with_limits(db.clone(), 8 * 1024 * 1024, 0, 8 * 1024 * 1024);
        let held_connection = db.acquire().await.expect("hold only database connection");
        let sink = DeploymentStepOutputSink::new(logs.clone(), task_id, step_id, vec![]);
        let chunk = vec![b'x'; DEPLOYMENT_LOG_WRITER_CHUNK_BYTES];

        for _ in 0..400 {
            sink.write(CommandOutputStream::Stdout, &chunk)
                .await
                .expect("queue output");
        }
        assert!(sink.dropped_bytes.load(Ordering::Acquire) > 0);

        drop(held_connection);
        sink.finish_redaction().await.expect("finish output sink");
        let snapshot = logs.finish(task_id, step_id).await.expect("finish log");
        assert!(snapshot.truncated);
        assert!(snapshot.dropped_bytes > 0);
        assert!(snapshot.stored_bytes <= 8 * 1024 * 1024);
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
        assert!(safe_package_name("../package.tar.gz").is_err());
        assert!(safe_package_name("package.tar.gz").is_ok());
    }

    #[test]
    fn staging_cleanup_failure_does_not_hide_cancellation_state() {
        let result = merge_execution_and_cleanup(
            Err(DeploymentRuntimeError::Canceled(
                "部署命令已取消".to_owned(),
            )),
            Err(DeploymentRuntimeError::Database("目录仍被占用".to_owned())),
        );

        assert!(matches!(
            result,
            Err(DeploymentRuntimeError::Canceled(message))
                if message.contains("部署命令已取消") && message.contains("目录仍被占用")
        ));
    }

    #[tokio::test]
    async fn removes_secret_staging_and_downloaded_release_after_execution() {
        let temp = tempdir().expect("create temp dir");
        let staging_root = temp.path().join("staging");
        let context = UnitExecutionContext {
            deployment_run_id: 21,
            task_id: 22,
            step_id: 23,
            environment_id: 2,
            target_node_ids: vec![7],
            item: crate::deployment_orchestrator::DeploymentPlanItem {
                unit_id: 4,
                unit_key: "api".to_owned(),
                unit_release_id: Some(5),
                release_version: Some("1.0.0".to_owned()),
                stage_no: 1,
                unit_order: 1,
                removal_order: 1,
                action: DeploymentAction::Deploy,
                reason: "test".to_owned(),
                target_fingerprint: "target".to_owned(),
                previous_fingerprint: String::new(),
            },
            cancellation: crate::deploy::CancellationSignal::new(),
        };
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
                work_dir: "/srv/orders/api".to_owned(),
                compose_content: "services: {}".to_owned(),
                scripts: BTreeMap::new(),
                health_check: serde_json::json!({}),
            },
            action: DeploymentAction::Deploy,
            release: None,
            target_nodes: Vec::new(),
            environment_variables: BTreeMap::from([(
                "APP_SECRET".to_owned(),
                "plain-text-secret".to_owned(),
            )]),
        };
        let unit_root = staging_root.join("1").join("2").join("4");
        let download_root = staging_root.join("downloads").join("21").join("4");
        fs::create_dir_all(&unit_root)
            .await
            .expect("create unit staging");
        fs::create_dir_all(&download_root)
            .await
            .expect("create download staging");
        fs::write(unit_root.join(".env"), b"APP_SECRET=plain-text-secret\n")
            .await
            .expect("write environment file");
        fs::write(download_root.join("api.tar.gz"), b"release")
            .await
            .expect("write downloaded release");

        cleanup_execution_staging(&staging_root, &context, &spec)
            .await
            .expect("cleanup execution staging");

        assert!(
            !fs::try_exists(unit_root)
                .await
                .expect("inspect unit staging")
        );
        assert!(
            !fs::try_exists(download_root)
                .await
                .expect("inspect download staging")
        );
    }

    #[tokio::test]
    async fn ssh_http_health_check_runs_on_target_node() {
        let temp = tempdir().expect("create temp dir");
        let runner = Arc::new(RecordingCommandRunner::default());
        let compose = ComposeExecutor::new(runner.clone());
        let ssh = SshExecutor::new(runner.clone());
        let target = SshTarget::new("deploy", "10.0.0.8", 22).expect("create SSH target");
        let prepared = PreparedUnitRuntime {
            root: temp.path().to_path_buf(),
            compose_path: temp.path().join("compose.yaml"),
            env_path: temp.path().join(".env"),
            package_path: None,
            script_paths: BTreeMap::new(),
        };
        let config = normalize_health_config("http", "http://127.0.0.1:8080/healthz", 5, 200)
            .expect("normalize health check");

        let result = run_node_health_check(
            &config,
            &prepared,
            Some(&target),
            &compose,
            &ssh,
            "/srv/app",
        )
        .await
        .expect("run remote health check");

        assert!(result.healthy);
        let commands = runner.commands.lock().expect("command lock");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "ssh");
        assert!(commands[0].args.iter().any(|arg| arg == "curl"));
    }
}
