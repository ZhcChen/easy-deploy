use std::{
    fs::{self, File},
    net::TcpListener,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, FixedOffset, NaiveDateTime, SecondsFormat, Utc};
use flate2::read::GzDecoder;
use fs2::available_space;
use rand_core::{OsRng, RngCore};
use serde_json::{Value as JsonValue, json};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tar::Archive;
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{self, MissedTickBehavior},
};
use tracing::{error, warn};
use url::Url;

use crate::{
    artifact_storage::{
        ArtifactStorageError, STORAGE_PROVIDER_ALIYUN_OSS, STORAGE_PROVIDER_LOCAL,
        normalize_checksum_sha256,
    },
    deploy::{
        ComposeCommandOutput, ComposeExecutor, DeployError, SshExecutor, SshTarget, SystemdExecutor,
    },
    health::{HealthCheckConfig, HealthCheckKind, normalize_health_config, run_health_check},
    platform::{PlatformConfigError, PlatformConfigService},
    runtimefs::{
        AppRuntimeConfig, BinaryRuntimeConfig, BinaryRuntimeFiles, BinaryRuntimeMetadata,
        CLEANUP_SCRIPT_FILE_NAME, CURRENT_RELEASE_FILE_NAME, DEPLOY_STAGE_SCRIPT_FILE_NAME,
        DeployScriptSet, META_DIR_NAME, POST_DEPLOY_SCRIPT_FILE_NAME, PRE_DEPLOY_SCRIPT_FILE_NAME,
        RELEASES_DIR_NAME, ReleaseRuntimeMetadata, RuntimeFs, RuntimeFsError, SCRIPTS_DIR_NAME,
        SWITCH_TRAFFIC_SCRIPT_FILE_NAME, SYSTEMD_DIR_NAME, TargetNodeMetadata,
    },
    tasks::{
        CreateTaskInput, RecordDeploymentRunInput, StartTaskStepInput, TaskError,
        TaskNodeResultInput, TaskService, active_task_status_label,
    },
};

const COMPOSE_TASK_QUEUE_CAPACITY: usize = 100;
const MIN_COMPOSE_FREE_SPACE_BYTES: u64 = 512 * 1024 * 1024;
pub const RELEASE_PACKAGE_PATTERN: &str = "{service_key}_version_{x_y_z}.tar.gz";
pub const RELEASE_PACKAGE_EXAMPLE: &str = "orders-api-prod_version_1_2_3.tar.gz";
pub const BINARY_PACKAGE_PATTERN: &str = RELEASE_PACKAGE_PATTERN;
pub const BINARY_PACKAGE_EXAMPLE: &str = RELEASE_PACKAGE_EXAMPLE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeployStrategy {
    RollingStopOnFailure,
    RollingContinue,
}

impl DeployStrategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::RollingStopOnFailure => "rolling_stop_on_failure",
            Self::RollingContinue => "rolling_continue",
        }
    }

    fn should_stop_after_failure(self) -> bool {
        self == Self::RollingStopOnFailure
    }

    fn label(self) -> &'static str {
        match self {
            Self::RollingStopOnFailure => "滚动部署，失败停止",
            Self::RollingContinue => "逐节点继续，最终汇总失败",
        }
    }
}

pub fn normalize_deploy_strategy(value: &str) -> Result<String, AppError> {
    let strategy = value.trim();
    if strategy.is_empty() {
        return Ok(DeployStrategy::RollingStopOnFailure.as_str().to_owned());
    }
    match strategy {
        "rolling_stop_on_failure" | "rolling_continue" => Ok(strategy.to_owned()),
        _ => Err(AppError::InvalidInput("部署策略不支持".to_owned())),
    }
}

pub fn normalize_release_source(value: &str) -> Result<String, AppError> {
    let source = value.trim();
    if source.is_empty() {
        return Ok("package_upload".to_owned());
    }
    match source {
        "manual" | "package_upload" => Ok(source.to_owned()),
        _ => Err(AppError::InvalidInput("发布来源不支持".to_owned())),
    }
}

fn parse_deploy_strategy(value: &str) -> DeployStrategy {
    match value {
        "rolling_continue" => DeployStrategy::RollingContinue,
        _ => DeployStrategy::RollingStopOnFailure,
    }
}

pub fn release_publish_mode_label(auto_queue_release: bool) -> &'static str {
    if auto_queue_release {
        "自动入队"
    } else {
        "手动发布"
    }
}

fn release_status_after_upload(auto_queue_release: bool) -> &'static str {
    if auto_queue_release {
        "queued"
    } else {
        "received"
    }
}

#[derive(Clone)]
pub struct AppService {
    db: SqlitePool,
    runtime_fs: RuntimeFs,
    compose: ComposeExecutor,
    systemd: SystemdExecutor,
    tasks: TaskService,
    compose_queue: ComposeTaskQueue,
    binary_queue: BinaryTaskQueue,
    release_dispatch_queue: ReleaseDispatchQueue,
    platform: PlatformConfigService,
}

#[derive(Debug)]
pub enum AppError {
    InvalidInput(String),
    Conflict(String),
    Internal(String),
}

impl AppError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Conflict(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for AppError {}

impl From<sqlx::Error> for AppError {
    fn from(value: sqlx::Error) -> Self {
        if let sqlx::Error::Database(err) = &value
            && err.is_unique_violation()
        {
            return Self::Conflict("应用标识已存在".to_owned());
        }
        Self::Internal(format!("应用数据操作失败: {value}"))
    }
}

impl From<RuntimeFsError> for AppError {
    fn from(value: RuntimeFsError) -> Self {
        match value {
            RuntimeFsError::InvalidInput(message) => Self::InvalidInput(message),
            RuntimeFsError::Io(message) => Self::Internal(message),
        }
    }
}

impl From<PlatformConfigError> for AppError {
    fn from(value: PlatformConfigError) -> Self {
        match value {
            PlatformConfigError::InvalidInput(message) => Self::InvalidInput(message),
            PlatformConfigError::Internal(message) => Self::Internal(message),
        }
    }
}

impl From<ArtifactStorageError> for AppError {
    fn from(value: ArtifactStorageError) -> Self {
        match value {
            ArtifactStorageError::InvalidInput(message)
            | ArtifactStorageError::Unsupported(message) => Self::InvalidInput(message),
        }
    }
}

impl From<DeployError> for AppError {
    fn from(value: DeployError) -> Self {
        match value {
            DeployError::InvalidInput(message) => Self::InvalidInput(message),
            DeployError::Command(message) => Self::Internal(message),
        }
    }
}

impl From<TaskError> for AppError {
    fn from(value: TaskError) -> Self {
        Self::Internal(value.message().to_owned())
    }
}

impl From<crate::health::HealthError> for AppError {
    fn from(value: crate::health::HealthError) -> Self {
        match value {
            crate::health::HealthError::InvalidInput(message) => Self::InvalidInput(message),
            crate::health::HealthError::CheckFailed(message) => Self::Internal(message),
        }
    }
}

#[derive(Clone)]
struct ComposeTaskQueue {
    sender: mpsc::Sender<ComposeTaskJob>,
}

#[derive(Clone)]
struct BinaryTaskQueue {
    sender: mpsc::Sender<BinaryTaskJob>,
}

#[derive(Clone)]
struct ReleaseDispatchQueue {
    sender: mpsc::Sender<i64>,
}

#[derive(Clone)]
struct ComposeWorkerContext {
    db: SqlitePool,
    runtime_fs: RuntimeFs,
    compose: ComposeExecutor,
    systemd: SystemdExecutor,
    tasks: TaskService,
    platform: PlatformConfigService,
    compose_queue: ComposeTaskQueue,
}

#[derive(Clone, Debug)]
struct ComposeTaskJob {
    task_id: i64,
    app_id: i64,
    release_id: Option<i64>,
    queue_id: Option<i64>,
    app_key: String,
    app_name: String,
    environment: String,
    compose_strategy: String,
    release_version: Option<String>,
    release_package_name: Option<String>,
    release_checksum_sha256: Option<String>,
    release_size_bytes: Option<i64>,
    release_storage_provider: Option<String>,
    release_storage_bucket: Option<String>,
    release_storage_object_key: Option<String>,
    release_storage_endpoint: Option<String>,
    config_snapshot_id: Option<i64>,
    config_revision_no: i64,
    deploy_strategy: DeployStrategy,
    action: ComposeTaskAction,
}

#[derive(Clone, Debug)]
struct BinaryTaskJob {
    task_id: i64,
    app_id: i64,
    release_id: Option<i64>,
    queue_id: Option<i64>,
    app_key: String,
    deploy_work_dir: String,
    unit_name: String,
    artifact_version: String,
    artifact_path: String,
    config_snapshot_id: Option<i64>,
    config_revision_no: i64,
    release_strategy: String,
    active_slot: String,
    base_port: i64,
    standby_port: i64,
    proxy_enabled: bool,
    proxy_kind: String,
    proxy_domain: String,
    proxy_config_path: String,
    deploy_strategy: DeployStrategy,
    action: BinaryTaskAction,
}

impl BinaryTaskJob {
    fn is_blue_green_restart(&self) -> bool {
        self.release_strategy == "blue_green" && self.action == BinaryTaskAction::Restart
    }

    fn target_slot(&self) -> &'static str {
        if self.is_blue_green_restart() {
            standby_slot(&self.active_slot)
        } else {
            normalized_slot(&self.active_slot)
        }
    }

    fn active_port(&self) -> i64 {
        match normalized_slot(&self.active_slot) {
            "green" => self.standby_port,
            _ => self.base_port,
        }
    }

    fn target_port(&self) -> i64 {
        match self.target_slot() {
            "green" => self.standby_port,
            _ => self.base_port,
        }
    }

    fn execution_unit_name(&self) -> String {
        if self.is_blue_green_restart() {
            binary_blue_green_unit_name(&self.unit_name, self.target_slot())
        } else {
            self.unit_name.clone()
        }
    }

    fn promoted_slot(&self, success: bool) -> Option<String> {
        if success && self.is_blue_green_restart() {
            Some(self.target_slot().to_owned())
        } else {
            None
        }
    }

    fn slot_health_endpoint(&self, endpoint: &str) -> String {
        if !self.is_blue_green_restart() {
            return endpoint.to_owned();
        }
        replace_endpoint_port(endpoint, self.active_port(), self.target_port())
    }
}

impl ComposeTaskQueue {
    fn start(
        db: SqlitePool,
        runtime_fs: RuntimeFs,
        compose: ComposeExecutor,
        systemd: SystemdExecutor,
        tasks: TaskService,
        platform: PlatformConfigService,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(COMPOSE_TASK_QUEUE_CAPACITY);
        let queue = Self { sender };
        let worker_queue = queue.clone();
        let worker_context = ComposeWorkerContext {
            db,
            runtime_fs,
            compose,
            systemd,
            tasks,
            platform,
            compose_queue: worker_queue,
        };
        let _worker = tokio::spawn(async move {
            compose_task_worker(receiver, worker_context).await;
        });
        queue
    }

    async fn enqueue(&self, job: ComposeTaskJob) -> Result<(), AppError> {
        self.sender
            .send(job)
            .await
            .map_err(|_| AppError::Internal("后台部署任务队列不可用".to_owned()))
    }
}

impl BinaryTaskQueue {
    fn start(
        db: SqlitePool,
        runtime_fs: RuntimeFs,
        compose: ComposeExecutor,
        systemd: SystemdExecutor,
        tasks: TaskService,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(COMPOSE_TASK_QUEUE_CAPACITY);
        let _worker = tokio::spawn(async move {
            binary_task_worker(receiver, db, runtime_fs, compose, systemd, tasks).await;
        });
        Self { sender }
    }

    async fn enqueue(&self, job: BinaryTaskJob) -> Result<(), AppError> {
        self.sender
            .send(job)
            .await
            .map_err(|_| AppError::Internal("后台二进制部署任务队列不可用".to_owned()))
    }
}

impl ReleaseDispatchQueue {
    fn start(
        db: SqlitePool,
        runtime_fs: RuntimeFs,
        tasks: TaskService,
        compose_queue: ComposeTaskQueue,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(COMPOSE_TASK_QUEUE_CAPACITY);
        let timer_sender = sender.clone();
        let schedule_db = db.clone();
        let _worker = tokio::spawn(async move {
            release_dispatch_worker(receiver, db, runtime_fs, tasks, compose_queue).await;
        });
        let _timer = tokio::spawn(async move {
            release_schedule_worker(schedule_db, timer_sender).await;
        });
        Self { sender }
    }

    async fn enqueue(&self, app_id: i64) -> Result<(), AppError> {
        self.sender
            .send(app_id)
            .await
            .map_err(|_| AppError::Internal("后台发布调度队列不可用".to_owned()))
    }
}

async fn compose_task_worker(
    mut receiver: mpsc::Receiver<ComposeTaskJob>,
    context: ComposeWorkerContext,
) {
    while let Some(job) = receiver.recv().await {
        run_compose_task_job(&context, job).await;
    }
}

async fn binary_task_worker(
    mut receiver: mpsc::Receiver<BinaryTaskJob>,
    db: SqlitePool,
    runtime_fs: RuntimeFs,
    compose: ComposeExecutor,
    systemd: SystemdExecutor,
    tasks: TaskService,
) {
    while let Some(job) = receiver.recv().await {
        run_binary_task_job(&db, &runtime_fs, &compose, &systemd, &tasks, job).await;
    }
}

async fn release_dispatch_worker(
    mut receiver: mpsc::Receiver<i64>,
    db: SqlitePool,
    runtime_fs: RuntimeFs,
    tasks: TaskService,
    compose_queue: ComposeTaskQueue,
) {
    while let Some(app_id) = receiver.recv().await {
        if let Err(err) =
            dispatch_next_release_for_app(&db, &runtime_fs, &tasks, &compose_queue, app_id).await
        {
            error!(app_id, error = %err, "failed to dispatch next release");
        }
    }
}

async fn release_schedule_worker(db: SqlitePool, sender: mpsc::Sender<i64>) {
    let mut interval = time::interval(Duration::from_secs(15));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let due_app_ids = match enqueue_due_scheduled_releases(&db).await {
            Ok(app_ids) => app_ids,
            Err(err) => {
                error!(error = %err, "failed to enqueue scheduled releases");
                continue;
            }
        };
        for app_id in due_app_ids {
            if sender.send(app_id).await.is_err() {
                break;
            }
        }
    }
}

#[derive(sqlx::FromRow)]
struct DueScheduledQueueRow {
    id: i64,
    app_id: i64,
}

async fn enqueue_due_scheduled_releases(db: &SqlitePool) -> Result<Vec<i64>, AppError> {
    let due_queue_items = sqlx::query_as::<_, DueScheduledQueueRow>(
        r#"
        SELECT id, app_id
        FROM app_release_queue
        WHERE scheduled_publish_at IS NOT NULL
          AND scheduled_publish_at != ''
          AND scheduled_publish_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
          AND status = 'scheduled'
        ORDER BY scheduled_publish_at ASC, queue_seq ASC, id ASC
        LIMIT 20
        "#,
    )
    .fetch_all(db)
    .await?;

    let mut app_ids = Vec::new();
    for item in due_queue_items {
        let result = sqlx::query(
            r#"
            UPDATE app_release_queue
            SET status = 'queued',
                message = '到达计划发布时间，已自动进入发布队列',
                scheduled_publish_at = NULL,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
              AND status = 'scheduled'
            "#,
        )
        .bind(item.id)
        .execute(db)
        .await?;
        if result.rows_affected() > 0 {
            app_ids.push(item.app_id);
        }
    }
    Ok(app_ids)
}

async fn dispatch_next_release_for_app(
    db: &SqlitePool,
    runtime_fs: &RuntimeFs,
    tasks: &TaskService,
    compose_queue: &ComposeTaskQueue,
    app_id: i64,
) -> Result<(), AppError> {
    let has_running = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(1)
        FROM app_release_queue
        WHERE app_id = ?1
          AND status = 'running'
        "#,
    )
    .bind(app_id)
    .fetch_one(db)
    .await?;
    if has_running > 0 {
        return Ok(());
    }

    let queue_item = sqlx::query_as::<_, PendingReleaseQueueItem>(
        r#"
        SELECT
            q.id,
            q.release_id,
            q.config_snapshot_id,
            r.version,
            r.version_code,
            r.package_name,
            r.package_path,
            r.checksum_sha256,
            r.size_bytes,
            r.storage_provider,
            r.storage_bucket,
            r.storage_object_key,
            r.storage_endpoint,
            r.published_at
        FROM app_release_queue q
        JOIN app_releases r ON r.id = q.release_id
        WHERE q.app_id = ?1
          AND q.status = 'queued'
        ORDER BY q.queue_seq ASC, q.id ASC
        LIMIT 1
        "#,
    )
    .bind(app_id)
    .fetch_optional(db)
    .await?;
    let Some(queue_item) = queue_item else {
        return Ok(());
    };

    let app = fetch_app_detail_by_id(db, app_id).await?;
    ensure_app_enabled(&app)?;

    let Some(snapshot_id) = queue_item.config_snapshot_id else {
        finish_release_queue_item(
            db,
            queue_item.id,
            queue_item.release_id,
            "failed",
            "发布队列缺少配置快照，无法执行部署",
        )
        .await?;
        return Ok(());
    };
    let snapshot = sqlx::query_as::<_, AppConfigSnapshotItem>(
        r#"
        SELECT
            id,
            revision_no,
            snapshot_kind,
            compose_content,
            env_content,
            artifact_version,
            config_hash,
            metadata,
            created_at
        FROM app_config_snapshots
        WHERE id = ?1
          AND app_id = ?2
        "#,
    )
    .bind(snapshot_id)
    .bind(app_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        AppError::InvalidInput(format!("发布队列绑定的配置快照 {snapshot_id} 不存在"))
    })?;

    let target_nodes = target_node_metadata_for_app(db, app_id).await?;
    ensure_has_enabled_targets(&app_target_nodes(db, app_id).await?)?;
    let runtime_root = runtime_fs.app_root(&app.app_key)?;
    let metadata_content =
        render_runtime_metadata(&app, target_nodes, &runtime_root.to_string_lossy(), None);
    runtime_fs
        .save_app_runtime_files_with_scripts(
            &app.app_key,
            &snapshot.compose_content,
            &snapshot.env_content,
            &metadata_content,
            &deploy_scripts_from_snapshot_metadata(&snapshot.metadata),
        )
        .await?;

    let task_id = tasks
        .create_task(CreateTaskInput {
            task_kind: "release.deploy".to_owned(),
            title: format!("发布版本 {} {}", queue_item.version, app.name),
            app_id: Some(app.id),
            release_id: Some(queue_item.release_id),
            node_id: None,
            created_by: "release-queue".to_owned(),
        })
        .await?;
    if !mark_release_queue_running(db, queue_item.id, task_id).await? {
        tasks
            .fail_task(task_id, "发布队列状态已变化，无法继续派发")
            .await?;
        return Ok(());
    }
    sqlx::query(
        r#"
        UPDATE app_releases
        SET status = 'deploying',
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(queue_item.release_id)
    .execute(db)
    .await?;
    tasks
        .append_log(
            task_id,
            "system",
            &format!(
                "版本 {} 已进入串行发布队列，队列项 #{}",
                queue_item.version, queue_item.id
            ),
        )
        .await?;
    tasks
        .append_log(
            task_id,
            "system",
            &format!("运行配置版本: config#{}", snapshot.revision_no),
        )
        .await?;
    tasks
        .append_log(
            task_id,
            "system",
            &format!(
                "版本包: {}，versionCode: {}，发布时间: {}",
                queue_item.package_path, queue_item.version_code, queue_item.published_at
            ),
        )
        .await?;
    update_runtime_states_in_db(&RuntimeStatesUpdate {
        db,
        app_id: app.id,
        runtime_status: "deploying",
        service_count: None,
        active_version: None,
        message: "版本已进入串行发布执行阶段",
        task_id: Some(task_id),
        touch_deploy_time: false,
    })
    .await?;

    let compose_strategy = load_app_compose_strategy(db, app.id).await?;
    let job = ComposeTaskJob {
        task_id,
        app_id: app.id,
        release_id: Some(queue_item.release_id),
        queue_id: Some(queue_item.id),
        app_key: app.app_key,
        app_name: app.name,
        environment: app.environment,
        compose_strategy,
        config_snapshot_id: Some(snapshot.id),
        config_revision_no: snapshot.revision_no,
        release_version: Some(queue_item.version),
        release_package_name: Some(queue_item.package_name),
        release_checksum_sha256: Some(queue_item.checksum_sha256),
        release_size_bytes: Some(queue_item.size_bytes),
        release_storage_provider: Some(queue_item.storage_provider),
        release_storage_bucket: Some(queue_item.storage_bucket),
        release_storage_object_key: Some(queue_item.storage_object_key),
        release_storage_endpoint: Some(queue_item.storage_endpoint),
        deploy_strategy: parse_deploy_strategy(&app.deploy_strategy),
        action: ComposeTaskAction::Up,
    };
    if let Err(err) = compose_queue.enqueue(job).await {
        finish_release_queue_item(
            db,
            queue_item.id,
            queue_item.release_id,
            "failed",
            err.message(),
        )
        .await?;
        tasks.fail_task(task_id, err.message()).await?;
        return Err(err);
    }
    Ok(())
}

async fn run_compose_task_job(context: &ComposeWorkerContext, job: ComposeTaskJob) {
    let db = &context.db;
    let runtime_fs = &context.runtime_fs;
    let compose = &context.compose;
    let systemd = &context.systemd;
    let tasks = &context.tasks;
    let platform = &context.platform;
    let compose_queue = &context.compose_queue;

    match tasks
        .mark_running(job.task_id, "部署前预检", "preflight")
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            error!(
                task_id = job.task_id,
                error = %err,
                "failed to mark compose task running"
            );
            return;
        }
    }

    let work_dir = match runtime_fs.app_root(&job.app_key) {
        Ok(work_dir) => work_dir,
        Err(err) => {
            fail_compose_task(db, tasks, job.app_id, job.task_id, err.message()).await;
            return;
        }
    };
    if !work_dir.is_dir() {
        let message = format!("Compose 工作目录不存在: {}", work_dir.to_string_lossy());
        fail_compose_task(db, tasks, job.app_id, job.task_id, &message).await;
        return;
    }
    if !work_dir.join("compose.yaml").is_file() {
        let message = format!(
            "Compose 配置文件不存在: {}",
            work_dir.join("compose.yaml").to_string_lossy()
        );
        fail_compose_task(db, tasks, job.app_id, job.task_id, &message).await;
        return;
    }

    let target_nodes = match app_target_nodes(db, job.app_id).await {
        Ok(nodes) if !nodes.is_empty() => nodes,
        Ok(_) => {
            fail_compose_task(
                db,
                tasks,
                job.app_id,
                job.task_id,
                "Compose 应用没有绑定目标节点",
            )
            .await;
            return;
        }
        Err(err) => {
            fail_compose_task(db, tasks, job.app_id, job.task_id, err.message()).await;
            return;
        }
    };

    let ssh = systemd.ssh_executor();
    let mut outputs = Vec::new();
    let mut node_messages = Vec::new();
    let mut overall_success = true;
    let execution_context = ComposeTaskExecutionContext {
        db,
        compose,
        systemd,
        ssh: &ssh,
        tasks,
        platform,
        task_id: job.task_id,
        runtime_work_dir: &work_dir,
        job: &job,
    };

    let mut stop_after_node_id = None;
    for node in &target_nodes {
        let result = run_compose_task_on_node(&execution_context, node).await;
        match result {
            Ok(result) => {
                overall_success &= result.success;
                record_task_node_result_best_effort(
                    tasks,
                    job.task_id,
                    node,
                    if result.success { "success" } else { "failed" },
                    &result.message,
                    result.outputs.len(),
                )
                .await;
                node_messages.push(format!("{}: {}", node.name, result.message));
                outputs.extend(result.outputs);

                let deployment_status = if result.success { "success" } else { "failed" };
                let runtime_status = job.action.runtime_status(result.success, deployment_status);
                let service_count = if result.success {
                    Some(if job.action == ComposeTaskAction::Down {
                        0
                    } else {
                        runtime_service_count(&work_dir)
                    })
                } else {
                    None
                };
                let active_version = if result.success {
                    Some(
                        job.release_version
                            .clone()
                            .unwrap_or_else(|| format!("task-{}", job.task_id)),
                    )
                } else {
                    None
                };
                update_runtime_state_for_node_best_effort(RuntimeStateUpdate {
                    db,
                    app_id: job.app_id,
                    node_id: node.id,
                    runtime_status,
                    service_count,
                    active_version: active_version.as_deref(),
                    message: &result.message,
                    task_id: Some(job.task_id),
                    touch_deploy_time: result.success,
                })
                .await;

                if !result.success {
                    if job.deploy_strategy.should_stop_after_failure() {
                        stop_after_node_id = Some(node.id);
                        break;
                    }
                    continue;
                }
            }
            Err(err) => {
                overall_success = false;
                let message = err.message().to_owned();
                record_task_node_result_best_effort(
                    tasks,
                    job.task_id,
                    node,
                    "failed",
                    &message,
                    0,
                )
                .await;
                node_messages.push(format!("{}: {}", node.name, message));
                update_runtime_state_for_node_best_effort(RuntimeStateUpdate {
                    db,
                    app_id: job.app_id,
                    node_id: node.id,
                    runtime_status: "unhealthy",
                    service_count: None,
                    active_version: None,
                    message: &message,
                    task_id: Some(job.task_id),
                    touch_deploy_time: false,
                })
                .await;
                if job.deploy_strategy.should_stop_after_failure() {
                    stop_after_node_id = Some(node.id);
                    break;
                }
            }
        }
    }
    mark_unexecuted_nodes_after_failure(
        tasks,
        job.task_id,
        db,
        job.app_id,
        &target_nodes,
        stop_after_node_id,
        &mut node_messages,
    )
    .await;

    if overall_success && let Some(release_version) = job.release_version.as_deref() {
        match runtime_fs
            .mark_current_release(&job.app_key, release_version)
            .await
        {
            Ok(result) => {
                if let Err(err) = tasks
                    .append_log(
                        job.task_id,
                        "system",
                        &format!(
                            "已更新当前生效版本指针: {}",
                            result.current_file.to_string_lossy()
                        ),
                    )
                    .await
                {
                    error!(
                        task_id = job.task_id,
                        error = %err,
                        "failed to append current release pointer log"
                    );
                }
                if let Err(err) = sync_current_release_pointer_to_ssh_targets(
                    tasks,
                    job.task_id,
                    &ssh,
                    &work_dir,
                    &job,
                    &target_nodes,
                )
                .await
                {
                    overall_success = false;
                    let message = err.message().to_owned();
                    node_messages.push(message.clone());
                    if let Err(log_err) = tasks.append_log(job.task_id, "stderr", &message).await {
                        error!(
                            task_id = job.task_id,
                            error = %log_err,
                            "failed to append current release pointer sync error"
                        );
                    }
                }
            }
            Err(err) => {
                overall_success = false;
                let message = format!("更新当前生效版本指针失败: {}", err.message());
                node_messages.push(message.clone());
                if let Err(log_err) = tasks.append_log(job.task_id, "stderr", &message).await {
                    error!(
                        task_id = job.task_id,
                        error = %log_err,
                        "failed to append current release pointer error"
                    );
                }
            }
        }
    }

    let final_output = merge_command_outputs(outputs, overall_success, "Compose 部署");
    let deployment_status = if overall_success { "success" } else { "failed" };
    let deployment_message = if node_messages.is_empty() {
        friendly_command_error(&final_output.output, "命令没有输出")
    } else {
        node_messages.join("；")
    };
    let final_output = prepend_failure_context(final_output, &deployment_message);
    let artifact_version = job.release_version.as_deref().unwrap_or("");
    let deploy_version_label = job
        .release_version
        .clone()
        .unwrap_or_else(|| format!("task-{}", job.task_id));
    let deploy_action = if job.release_id.is_some() {
        "release_deploy"
    } else {
        job.action.deploy_action()
    };

    if let Err(err) = tasks
        .record_deployment_run(RecordDeploymentRunInput {
            app_id: job.app_id,
            task_id: job.task_id,
            release_id: job.release_id,
            deploy_action,
            status: deployment_status,
            message: &deployment_message,
            config_snapshot_id: job.config_snapshot_id,
            config_revision_no: job.config_revision_no,
            artifact_version,
        })
        .await
    {
        error!(
            task_id = job.task_id,
            error = %err,
            "failed to record deployment run"
        );
    };
    if overall_success {
        match record_deploy_config_snapshot(
            db,
            job.app_id,
            &work_dir,
            if job.release_id.is_some() {
                "release_deploy"
            } else {
                "compose_task"
            },
            &deploy_version_label,
            artifact_version,
            None,
        )
        .await
        {
            Ok(snapshot) => {
                if let Err(err) = bind_deployment_run_snapshot(
                    db,
                    job.app_id,
                    job.task_id,
                    &snapshot,
                    artifact_version,
                )
                .await
                {
                    error!(
                        task_id = job.task_id,
                        error = %err,
                        "failed to bind compose deployment run snapshot"
                    );
                }
            }
            Err(err) => {
                error!(
                    task_id = job.task_id,
                    error = %err,
                    "failed to record compose deploy config snapshot"
                );
            }
        }
    }
    if let Err(err) = tasks
        .finish_with_compose_output(job.task_id, &final_output)
        .await
    {
        error!(
            task_id = job.task_id,
            error = %err,
            "failed to finish compose task"
        );
    }
    if let (Some(queue_id), Some(release_id)) = (job.queue_id, job.release_id) {
        let queue_status = if overall_success { "success" } else { "failed" };
        if let Err(err) =
            finish_release_queue_item(db, queue_id, release_id, queue_status, &deployment_message)
                .await
        {
            error!(
                task_id = job.task_id,
                queue_id,
                release_id,
                error = %err,
                "failed to finish release queue item"
            );
        } else if overall_success
            && let Err(err) =
                dispatch_next_release_for_app(db, runtime_fs, tasks, compose_queue, job.app_id)
                    .await
        {
            error!(
                task_id = job.task_id,
                app_id = job.app_id,
                error = %err,
                "failed to dispatch next queued release"
            );
        }
    } else if overall_success
        && let Err(err) =
            dispatch_next_release_for_app(db, runtime_fs, tasks, compose_queue, job.app_id).await
    {
        error!(
            task_id = job.task_id,
            app_id = job.app_id,
            error = %err,
            "failed to dispatch queued release after compose task"
        );
    }
}

async fn run_binary_task_job(
    db: &SqlitePool,
    runtime_fs: &RuntimeFs,
    compose: &ComposeExecutor,
    systemd: &SystemdExecutor,
    tasks: &TaskService,
    job: BinaryTaskJob,
) {
    match tasks
        .mark_running(job.task_id, "systemd 操作", "preparing_files")
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            error!(
                task_id = job.task_id,
                error = %err,
                "failed to mark binary task running"
            );
            return;
        }
    }

    let work_dir = match runtime_fs.app_root(&job.app_key) {
        Ok(work_dir) => work_dir,
        Err(err) => {
            fail_binary_task(db, tasks, job.app_id, job.task_id, err.message()).await;
            return;
        }
    };
    if !work_dir.is_dir() {
        let message = format!("二进制工作目录不存在: {}", work_dir.to_string_lossy());
        fail_binary_task(db, tasks, job.app_id, job.task_id, &message).await;
        return;
    }

    let target_nodes = match app_target_nodes(db, job.app_id).await {
        Ok(nodes) if !nodes.is_empty() => nodes,
        Ok(_) => {
            fail_binary_task(
                db,
                tasks,
                job.app_id,
                job.task_id,
                "二进制应用没有绑定目标节点",
            )
            .await;
            return;
        }
        Err(err) => {
            fail_binary_task(db, tasks, job.app_id, job.task_id, err.message()).await;
            return;
        }
    };

    if job.release_strategy == "blue_green"
        && let Err(err) = tasks
            .append_log(
                job.task_id,
                "system",
                &binary_blue_green_job_plan_message(&job),
            )
            .await
    {
        fail_binary_task(db, tasks, job.app_id, job.task_id, err.message()).await;
        return;
    }

    let ssh = systemd.ssh_executor();
    let mut outputs = Vec::new();
    let mut node_messages = Vec::new();
    let mut overall_success = true;
    let execution_context = BinaryTaskExecutionContext {
        db,
        compose,
        systemd,
        ssh: &ssh,
        tasks,
        task_id: job.task_id,
        runtime_work_dir: &work_dir,
        job: &job,
    };

    let mut stop_after_node_id = None;
    let mut promoted_slot = None;
    for node in &target_nodes {
        let result = run_binary_task_on_node(&execution_context, node).await;

        match result {
            Ok(result) => {
                overall_success &= result.success;
                record_task_node_result_best_effort(
                    tasks,
                    job.task_id,
                    node,
                    if result.success { "success" } else { "failed" },
                    &result.message,
                    result.outputs.len(),
                )
                .await;
                node_messages.push(format!("{}: {}", node.name, result.message));
                outputs.extend(result.outputs);

                let deployment_status = if result.success { "success" } else { "failed" };
                let runtime_status = job.action.runtime_status(result.success, deployment_status);
                let service_count = if result.success {
                    Some(if job.action == BinaryTaskAction::Stop {
                        0
                    } else {
                        1
                    })
                } else {
                    None
                };
                let active_version = if result.success {
                    Some(job.artifact_version.as_str())
                } else {
                    None
                };
                update_runtime_state_for_node_best_effort(RuntimeStateUpdate {
                    db,
                    app_id: job.app_id,
                    node_id: node.id,
                    runtime_status,
                    service_count,
                    active_version,
                    message: &result.message,
                    task_id: Some(job.task_id),
                    touch_deploy_time: result.success,
                })
                .await;

                if result.success
                    && let Some(slot) = result.promoted_slot
                {
                    if promoted_slot
                        .as_deref()
                        .is_some_and(|existing| existing != slot)
                    {
                        overall_success = false;
                        node_messages.push(format!("节点 {} 返回的目标槽位不一致", node.name));
                    } else {
                        promoted_slot = Some(slot);
                    }
                }

                if !result.success {
                    if job.deploy_strategy.should_stop_after_failure() {
                        stop_after_node_id = Some(node.id);
                        break;
                    }
                    continue;
                }
            }
            Err(err) => {
                overall_success = false;
                let message = err.message().to_owned();
                record_task_node_result_best_effort(
                    tasks,
                    job.task_id,
                    node,
                    "failed",
                    &message,
                    0,
                )
                .await;
                node_messages.push(format!("{}: {}", node.name, message));
                update_runtime_state_for_node_best_effort(RuntimeStateUpdate {
                    db,
                    app_id: job.app_id,
                    node_id: node.id,
                    runtime_status: "unhealthy",
                    service_count: None,
                    active_version: None,
                    message: &message,
                    task_id: Some(job.task_id),
                    touch_deploy_time: false,
                })
                .await;
                if job.deploy_strategy.should_stop_after_failure() {
                    stop_after_node_id = Some(node.id);
                    break;
                }
            }
        }
    }
    mark_unexecuted_nodes_after_failure(
        tasks,
        job.task_id,
        db,
        job.app_id,
        &target_nodes,
        stop_after_node_id,
        &mut node_messages,
    )
    .await;

    if overall_success && let Some(slot) = promoted_slot.as_deref() {
        match switch_binary_proxy_to_targets(
            tasks,
            job.task_id,
            systemd,
            &ssh,
            &work_dir,
            &job,
            &target_nodes,
        )
        .await
        {
            Ok(proxy_outputs) => {
                outputs.extend(proxy_outputs);
                if job.proxy_enabled {
                    let message = format!(
                        "Blue/Green 反向代理已切换到 {slot}({})",
                        display_port(job.target_port())
                    );
                    node_messages.push(message.clone());
                    if let Err(err) = tasks.append_log(job.task_id, "system", &message).await {
                        overall_success = false;
                        node_messages.push(format!("记录反向代理切流日志失败: {}", err.message()));
                    }
                }
            }
            Err(err) => {
                overall_success = false;
                let message = format!(
                    "Blue/Green 反向代理切流失败，保留当前槽位 {}: {}",
                    job.active_slot,
                    err.message()
                );
                node_messages.push(message.clone());
                if let Err(log_err) = tasks.append_log(job.task_id, "system", &message).await {
                    error!(
                        task_id = job.task_id,
                        error = %log_err,
                        "failed to append proxy switch failure log"
                    );
                }
                match cleanup_binary_standby_slot(
                    tasks,
                    job.task_id,
                    systemd,
                    &ssh,
                    &work_dir,
                    &job,
                    &target_nodes,
                )
                .await
                {
                    Ok(cleanup_outputs) => {
                        outputs.extend(cleanup_outputs);
                        let cleanup_message = format!(
                            "已尝试停止本次启动的备用槽位 {}，旧槽位 {} 继续保留",
                            job.target_slot(),
                            job.active_slot
                        );
                        node_messages.push(cleanup_message.clone());
                        if let Err(log_err) = tasks
                            .append_log(job.task_id, "system", &cleanup_message)
                            .await
                        {
                            error!(
                                task_id = job.task_id,
                                error = %log_err,
                                "failed to append standby cleanup log"
                            );
                        }
                    }
                    Err(cleanup_err) => {
                        let cleanup_message = format!(
                            "停止备用槽位 {} 失败，请人工检查: {}",
                            job.target_slot(),
                            cleanup_err.message()
                        );
                        node_messages.push(cleanup_message.clone());
                        if let Err(log_err) = tasks
                            .append_log(job.task_id, "system", &cleanup_message)
                            .await
                        {
                            error!(
                                task_id = job.task_id,
                                error = %log_err,
                                "failed to append standby cleanup failure log"
                            );
                        }
                    }
                }
            }
        }
    }

    if overall_success && let Some(slot) = promoted_slot.as_deref() {
        match promote_binary_active_slot(db, runtime_fs, &job, &work_dir, slot).await {
            Ok(()) => {
                match sync_promoted_binary_runtime_to_targets(
                    tasks,
                    job.task_id,
                    &ssh,
                    &work_dir,
                    &job,
                    &target_nodes,
                )
                .await
                {
                    Ok(sync_outputs) => {
                        outputs.extend(sync_outputs);
                        let message = format!(
                            "Blue/Green 槽位已记录为 {slot}；已同步提升后的运行文件；旧槽未自动停止"
                        );
                        node_messages.push(message.clone());
                        if let Err(err) = tasks.append_log(job.task_id, "system", &message).await {
                            overall_success = false;
                            node_messages
                                .push(format!("记录 Blue/Green 槽位日志失败: {}", err.message()));
                        }
                    }
                    Err(err) => {
                        overall_success = false;
                        let message =
                            format!("同步提升后的 Blue/Green 运行文件失败: {}", err.message());
                        node_messages.push(message.clone());
                        if let Err(log_err) =
                            tasks.append_log(job.task_id, "system", &message).await
                        {
                            error!(
                                task_id = job.task_id,
                                error = %log_err,
                                "failed to append blue/green resync failure log"
                            );
                        }
                    }
                }
            }
            Err(err) => {
                overall_success = false;
                let message = format!("更新 Blue/Green 槽位失败: {}", err.message());
                node_messages.push(message.clone());
                if let Err(log_err) = tasks.append_log(job.task_id, "system", &message).await {
                    error!(
                        task_id = job.task_id,
                        error = %log_err,
                        "failed to append blue/green promotion failure log"
                    );
                }
            }
        }
    }

    let final_output = merge_command_outputs(outputs, overall_success, "二进制部署");
    let deployment_status = if overall_success { "success" } else { "failed" };
    let deployment_message = if node_messages.is_empty() {
        friendly_command_error(&final_output.output, "命令没有输出")
    } else {
        node_messages.join("；")
    };
    let final_output = prepend_failure_context(final_output, &deployment_message);

    if let Err(err) = tasks
        .record_deployment_run(RecordDeploymentRunInput {
            app_id: job.app_id,
            task_id: job.task_id,
            release_id: None,
            deploy_action: job.action.deploy_action(),
            status: deployment_status,
            message: &deployment_message,
            config_snapshot_id: job.config_snapshot_id,
            config_revision_no: job.config_revision_no,
            artifact_version: &job.artifact_version,
        })
        .await
    {
        error!(
            task_id = job.task_id,
            error = %err,
            "failed to record binary deployment run"
        );
    };
    if overall_success {
        let binary_config = fetch_binary_config_for_app(db, job.app_id).await.ok();
        match record_deploy_config_snapshot(
            db,
            job.app_id,
            &work_dir,
            "binary_task",
            &job.artifact_version,
            &job.artifact_version,
            binary_config.as_ref(),
        )
        .await
        {
            Ok(snapshot) => {
                if let Err(err) = bind_deployment_run_snapshot(
                    db,
                    job.app_id,
                    job.task_id,
                    &snapshot,
                    &job.artifact_version,
                )
                .await
                {
                    error!(
                        task_id = job.task_id,
                        error = %err,
                        "failed to bind binary deployment run snapshot"
                    );
                }
            }
            Err(err) => {
                error!(
                    task_id = job.task_id,
                    error = %err,
                    "failed to record binary deploy config snapshot"
                );
            }
        }
    }
    if let Err(err) = tasks
        .finish_with_compose_output(job.task_id, &final_output)
        .await
    {
        error!(
            task_id = job.task_id,
            error = %err,
            "failed to finish binary task"
        );
    }
    if let (Some(queue_id), Some(release_id)) = (job.queue_id, job.release_id) {
        let queue_status = if overall_success { "success" } else { "failed" };
        if let Err(err) =
            finish_release_queue_item(db, queue_id, release_id, queue_status, &deployment_message)
                .await
        {
            error!(
                task_id = job.task_id,
                queue_id,
                release_id,
                error = %err,
                "failed to finish release queue item"
            );
        }
    }
}

async fn fail_compose_task(
    db: &SqlitePool,
    tasks: &TaskService,
    app_id: i64,
    task_id: i64,
    message: &str,
) {
    update_runtime_states_best_effort(RuntimeStatesUpdate {
        db,
        app_id,
        runtime_status: "unhealthy",
        service_count: None,
        active_version: None,
        message,
        task_id: Some(task_id),
        touch_deploy_time: false,
    })
    .await;
    if let Ok(Some((queue_id, release_id))) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT q.id, q.release_id
        FROM app_release_queue q
        WHERE q.task_id = ?1
          AND q.status IN ('queued', 'running')
        LIMIT 1
        "#,
    )
    .bind(task_id)
    .fetch_optional(db)
    .await
        && let Err(err) =
            finish_release_queue_item(db, queue_id, release_id, "failed", message).await
    {
        error!(
            task_id,
            queue_id,
            release_id,
            error = %err,
            "failed to mark compose release queue failed"
        );
    }
    if let Err(err) = tasks.fail_task(task_id, message).await {
        error!(
            task_id,
            error = %err,
            "failed to mark compose task failed"
        );
    }
}

async fn fail_binary_task(
    db: &SqlitePool,
    tasks: &TaskService,
    app_id: i64,
    task_id: i64,
    message: &str,
) {
    update_runtime_states_best_effort(RuntimeStatesUpdate {
        db,
        app_id,
        runtime_status: "unhealthy",
        service_count: None,
        active_version: None,
        message,
        task_id: Some(task_id),
        touch_deploy_time: false,
    })
    .await;
    if let Ok(Some((queue_id, release_id))) = sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT q.id, q.release_id
        FROM app_release_queue q
        WHERE q.task_id = ?1
          AND q.status IN ('queued', 'running')
        LIMIT 1
        "#,
    )
    .bind(task_id)
    .fetch_optional(db)
    .await
        && let Err(err) =
            finish_release_queue_item(db, queue_id, release_id, "failed", message).await
    {
        error!(
            task_id,
            queue_id,
            release_id,
            error = %err,
            "failed to mark release queue failed"
        );
    }
    if let Err(err) = tasks.fail_task(task_id, message).await {
        error!(
            task_id,
            error = %err,
            "failed to mark binary task failed"
        );
    }
}

#[derive(Debug)]
struct ComposeNodeTaskResult {
    success: bool,
    message: String,
    outputs: Vec<ComposeCommandOutput>,
}

struct ComposeTaskExecutionContext<'a> {
    db: &'a SqlitePool,
    compose: &'a ComposeExecutor,
    systemd: &'a SystemdExecutor,
    ssh: &'a SshExecutor,
    tasks: &'a TaskService,
    platform: &'a PlatformConfigService,
    task_id: i64,
    runtime_work_dir: &'a Path,
    job: &'a ComposeTaskJob,
}

#[derive(Clone, Copy)]
enum DeployScriptSlot {
    PreDeploy,
    Deploy,
    PostDeploy,
    SwitchTraffic,
    Cleanup,
}

impl DeployScriptSlot {
    fn phase(self) -> &'static str {
        match self {
            Self::PreDeploy => "pre_deploy",
            Self::Deploy => "deploy",
            Self::PostDeploy => "post_deploy",
            Self::SwitchTraffic => "switch_traffic",
            Self::Cleanup => "cleanup",
        }
    }

    fn step_key(self) -> &'static str {
        match self {
            Self::PreDeploy => "script.pre_deploy",
            Self::Deploy => "script.deploy",
            Self::PostDeploy => "script.post_deploy",
            Self::SwitchTraffic => "script.switch_traffic",
            Self::Cleanup => "script.cleanup",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::PreDeploy => "执行发布前脚本",
            Self::Deploy => "执行部署脚本",
            Self::PostDeploy => "执行发布后脚本",
            Self::SwitchTraffic => "执行切流脚本",
            Self::Cleanup => "执行清理脚本",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::PreDeploy => PRE_DEPLOY_SCRIPT_FILE_NAME,
            Self::Deploy => DEPLOY_STAGE_SCRIPT_FILE_NAME,
            Self::PostDeploy => POST_DEPLOY_SCRIPT_FILE_NAME,
            Self::SwitchTraffic => SWITCH_TRAFFIC_SCRIPT_FILE_NAME,
            Self::Cleanup => CLEANUP_SCRIPT_FILE_NAME,
        }
    }

    fn script_content(self, scripts: &DeployScriptSet) -> &str {
        match self {
            Self::PreDeploy => &scripts.pre_deploy,
            Self::Deploy => &scripts.deploy,
            Self::PostDeploy => &scripts.post_deploy,
            Self::SwitchTraffic => &scripts.switch_traffic,
            Self::Cleanup => &scripts.cleanup,
        }
    }

    fn relative_path(self) -> String {
        format!("{META_DIR_NAME}/{SCRIPTS_DIR_NAME}/{}", self.file_name())
    }
}

async fn run_compose_task_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
) -> Result<ComposeNodeTaskResult, AppError> {
    context
        .tasks
        .append_log(
            context.task_id,
            "system",
            &format!(
                "开始在节点 {}({}) 执行 Compose 任务",
                node.name, node.node_key
            ),
        )
        .await?;

    context
        .tasks
        .update_phase(context.task_id, "preflight")
        .await?;
    let preflight_step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        "node.preflight",
        &format!("节点 {} 预检", node.name),
        "check node availability",
    )
    .await?;
    if let Some(message) =
        block_unavailable_node_preflight(context.tasks, context.task_id, node).await?
    {
        fail_task_step_message(context.tasks, context.task_id, preflight_step_id, &message).await?;
        return Ok(ComposeNodeTaskResult {
            success: false,
            message,
            outputs: Vec::new(),
        });
    }
    finish_task_step(
        context.tasks,
        context.task_id,
        preflight_step_id,
        true,
        None,
        "节点预检通过",
    )
    .await?;

    let mut outputs = if node.node_type == "ssh" {
        context
            .tasks
            .update_phase(context.task_id, "preparing_files")
            .await?;
        let sync_step_id = start_task_step(
            context.tasks,
            context.task_id,
            Some(node),
            "compose.sync",
            &format!("同步 Compose 文件到 {}", node.name),
            "mkdir/copy compose runtime files",
        )
        .await?;
        match sync_compose_runtime_to_ssh_target(
            context.tasks,
            context.task_id,
            Some(sync_step_id),
            context.ssh,
            context.runtime_work_dir,
            context.job,
            node,
        )
        .await
        {
            Ok(outputs) => {
                finish_task_step(
                    context.tasks,
                    context.task_id,
                    sync_step_id,
                    true,
                    Some(0),
                    "Compose 运行文件同步完成",
                )
                .await?;
                outputs
            }
            Err(err) => {
                fail_task_step_message(context.tasks, context.task_id, sync_step_id, err.message())
                    .await?;
                return Err(err);
            }
        }
    } else {
        Vec::new()
    };

    if let Some(output) = download_release_package_on_node(context, node).await? {
        let success = output.success;
        let message = if success {
            "版本包下载和校验完成".to_owned()
        } else {
            friendly_command_error(&output.output, "版本包下载或校验失败")
        };
        outputs.push(output);
        if !success {
            return Ok(ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            });
        }
    }

    context
        .tasks
        .update_phase(context.task_id, "preflight")
        .await?;
    let compose_preflight_step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        "compose.config",
        &format!("校验 {} 的 Compose 配置", node.name),
        "docker info && docker compose config",
    )
    .await?;
    let preflight =
        run_compose_preflight_on_node(context, node, Some(compose_preflight_step_id)).await?;
    finish_task_step_result(
        context.tasks,
        context.task_id,
        compose_preflight_step_id,
        &preflight,
    )
    .await?;
    if !preflight.success {
        outputs.extend(preflight.outputs);
        return Ok(ComposeNodeTaskResult {
            success: false,
            message: preflight.message,
            outputs,
        });
    }
    outputs.extend(preflight.outputs);

    let scripts = deploy_scripts_from_runtime_dir(context.runtime_work_dir);
    if let Some(output) =
        run_deploy_script_slot_on_node(context, node, DeployScriptSlot::PreDeploy, &scripts).await?
    {
        let success = output.success;
        let message = deploy_script_step_message(DeployScriptSlot::PreDeploy, &output);
        outputs.push(output);
        if !success {
            return Ok(ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            });
        }
    }

    context
        .tasks
        .update_phase(context.task_id, "deploy")
        .await?;
    let deploy_script_configured = !DeployScriptSlot::Deploy
        .script_content(&scripts)
        .trim()
        .is_empty();
    let action_step_key = if deploy_script_configured {
        DeployScriptSlot::Deploy.step_key()
    } else {
        "compose.action"
    };
    let action_step_title = if deploy_script_configured {
        format!("{} {}", node.name, DeployScriptSlot::Deploy.title())
    } else {
        format!("{} {}", node.name, context.job.action.label())
    };
    let action_step_command = if deploy_script_configured {
        format!("sh {}", DeployScriptSlot::Deploy.relative_path())
    } else {
        compose_action_command_label(context.job.action).to_owned()
    };
    let action_step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        action_step_key,
        &action_step_title,
        &action_step_command,
    )
    .await?;
    let output = if deploy_script_configured {
        run_deploy_script_command_on_node(context, node, DeployScriptSlot::Deploy).await?
    } else {
        run_compose_action_on_node(context, node).await?
    };
    append_step_command_output(context.tasks, context.task_id, action_step_id, &output).await?;
    let command_success = output.success;
    let script_command_message = deploy_script_configured
        .then(|| deploy_script_step_message(DeployScriptSlot::Deploy, &output));
    let mut message = friendly_command_error(&output.output, "命令没有输出");
    if let Some(script_message) = script_command_message {
        message = script_message;
    }
    finish_task_step(
        context.tasks,
        context.task_id,
        action_step_id,
        command_success,
        output.status_code.map(i64::from),
        &message,
    )
    .await?;
    outputs.push(output);

    if command_success
        && let Some(output) =
            run_deploy_script_slot_on_node(context, node, DeployScriptSlot::PostDeploy, &scripts)
                .await?
    {
        let success = output.success;
        message = deploy_script_step_message(DeployScriptSlot::PostDeploy, &output);
        outputs.push(output);
        if !success {
            return Ok(ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            });
        }
    }

    if command_success && context.job.action.runs_health_check() {
        context
            .tasks
            .update_phase(context.task_id, "healthchecking")
            .await?;
        let health_step_id = start_task_step(
            context.tasks,
            context.task_id,
            Some(node),
            "compose.healthcheck",
            &format!("{} 健康检查", node.name),
            "health check",
        )
        .await?;
        let health = run_compose_health_check_on_node(context, node, Some(health_step_id)).await?;
        message = health.message.clone();
        finish_task_step_result(context.tasks, context.task_id, health_step_id, &health).await?;
        outputs.extend(health.outputs);
        if !health.success {
            return Ok(ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            });
        }
    }

    if command_success && context.job.action == ComposeTaskAction::Up {
        if let Some(output) =
            run_deploy_script_slot_on_node(context, node, DeployScriptSlot::SwitchTraffic, &scripts)
                .await?
        {
            let success = output.success;
            message = deploy_script_step_message(DeployScriptSlot::SwitchTraffic, &output);
            outputs.push(output);
            if !success {
                return Ok(ComposeNodeTaskResult {
                    success: false,
                    message,
                    outputs,
                });
            }
        }
        if let Some(output) =
            run_deploy_script_slot_on_node(context, node, DeployScriptSlot::Cleanup, &scripts)
                .await?
        {
            let success = output.success;
            message = deploy_script_step_message(DeployScriptSlot::Cleanup, &output);
            outputs.push(output);
            if !success {
                return Ok(ComposeNodeTaskResult {
                    success: false,
                    message,
                    outputs,
                });
            }
        }
    }

    Ok(ComposeNodeTaskResult {
        success: command_success,
        message,
        outputs,
    })
}

async fn run_compose_action_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
) -> Result<ComposeCommandOutput, AppError> {
    match node.node_type.as_str() {
        "local" => match context.job.action {
            ComposeTaskAction::Up => {
                context
                    .compose
                    .up(context.runtime_work_dir.to_path_buf())
                    .await
            }
            ComposeTaskAction::Down => {
                context
                    .compose
                    .down(context.runtime_work_dir.to_path_buf())
                    .await
            }
            ComposeTaskAction::Restart => {
                context
                    .compose
                    .restart(context.runtime_work_dir.to_path_buf())
                    .await
            }
        }
        .map_err(AppError::from),
        "ssh" => {
            let target = node.ssh_target()?;
            let remote_work_dir = compose_node_deploy_work_dir(context.job, node);
            match context.job.action {
                ComposeTaskAction::Up => {
                    context
                        .ssh
                        .compose_up(
                            &target,
                            context.runtime_work_dir.to_path_buf(),
                            &remote_work_dir,
                        )
                        .await
                }
                ComposeTaskAction::Down => {
                    context
                        .ssh
                        .compose_down(
                            &target,
                            context.runtime_work_dir.to_path_buf(),
                            &remote_work_dir,
                        )
                        .await
                }
                ComposeTaskAction::Restart => {
                    context
                        .ssh
                        .compose_restart(
                            &target,
                            context.runtime_work_dir.to_path_buf(),
                            &remote_work_dir,
                        )
                        .await
                }
            }
            .map_err(AppError::from)
        }
        _ => Err(AppError::InvalidInput(format!(
            "节点 {} 的类型 {} 不支持 Compose 部署",
            node.name, node.node_type
        ))),
    }
}

async fn download_release_package_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
) -> Result<Option<ComposeCommandOutput>, AppError> {
    if context.job.action != ComposeTaskAction::Up {
        return Ok(None);
    }
    let Some(storage_provider) = context.job.release_storage_provider.as_deref() else {
        return Ok(None);
    };
    if storage_provider != STORAGE_PROVIDER_ALIYUN_OSS {
        return Ok(None);
    }
    let release_version = context
        .job
        .release_version
        .as_deref()
        .ok_or_else(|| AppError::Internal("OSS 发布缺少版本号".to_owned()))?;
    let package_name = context
        .job
        .release_package_name
        .as_deref()
        .ok_or_else(|| AppError::Internal("OSS 发布缺少版本包文件名".to_owned()))?;
    let checksum = context
        .job
        .release_checksum_sha256
        .as_deref()
        .ok_or_else(|| AppError::Internal("OSS 发布缺少 SHA-256 校验值".to_owned()))?;
    let size_bytes = context
        .job
        .release_size_bytes
        .ok_or_else(|| AppError::Internal("OSS 发布缺少版本包大小".to_owned()))?;
    let object_key = context
        .job
        .release_storage_object_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Internal("OSS 发布缺少 ObjectKey".to_owned()))?;
    let bucket = context
        .job
        .release_storage_bucket
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Internal("OSS 发布缺少 Bucket".to_owned()))?;
    let endpoint = context
        .job
        .release_storage_endpoint
        .as_deref()
        .unwrap_or("");
    context
        .tasks
        .update_phase(context.task_id, "preparing_files")
        .await?;
    let step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        "artifact.download",
        &format!("{} 下载版本包", node.name),
        &format!("oss://{bucket}/{object_key}"),
    )
    .await?;
    let output_result: Result<ComposeCommandOutput, AppError> = async {
        let platform_config = context.platform.config().await?;
        if !platform_config.artifact_storage.is_aliyun_oss() {
            return Err(AppError::InvalidInput(
                "当前平台制品存储未启用阿里云 OSS，无法下载 OSS 版本包".to_owned(),
            ));
        }
        let mut oss = platform_config.artifact_storage.aliyun_oss.clone();
        oss.bucket = bucket.to_owned();
        if !endpoint.trim().is_empty() {
            oss.endpoint = endpoint.to_owned();
        }
        let oss = oss.normalize()?;
        let signed = oss.presign_download(object_key)?;
        let app_dir = compose_node_runtime_dir_for_script(context, node);
        let release_dir =
            compose_script_join(&app_dir, &format!("{RELEASES_DIR_NAME}/{release_version}"));
        let package_path = compose_script_join(&release_dir, package_name);
        if release_dir.is_empty() || package_path.is_empty() {
            return Err(AppError::Internal("无法计算目标节点版本包路径".to_owned()));
        }
        let command = render_release_package_download_command(
            &release_dir,
            &package_path,
            &signed.url,
            checksum,
            size_bytes,
        );
        let display_command =
            format!("download oss://{bucket}/{object_key} -> {package_path} && verify sha256");
        match node.node_type.as_str() {
            "local" => context
                .compose
                .run_shell_redacted(
                    context.runtime_work_dir.to_path_buf(),
                    &command,
                    &display_command,
                )
                .await
                .map_err(AppError::from),
            "ssh" => {
                let target = node.ssh_target()?;
                context
                    .ssh
                    .run_shell_redacted(
                        &target,
                        context.runtime_work_dir.to_path_buf(),
                        &command,
                        &display_command,
                    )
                    .await
                    .map_err(AppError::from)
            }
            _ => Err(AppError::InvalidInput(format!(
                "节点 {} 的类型 {} 不支持版本包下载",
                node.name, node.node_type
            ))),
        }
    }
    .await;
    match output_result {
        Ok(output) => {
            append_step_command_output(context.tasks, context.task_id, step_id, &output).await?;
            let message = if output.success {
                "版本包下载和校验完成"
            } else {
                "版本包下载或校验失败"
            };
            finish_task_step(
                context.tasks,
                context.task_id,
                step_id,
                output.success,
                output.status_code.map(i64::from),
                message,
            )
            .await?;
            Ok(Some(output))
        }
        Err(err) => {
            fail_task_step_message(context.tasks, context.task_id, step_id, err.message()).await?;
            Err(err)
        }
    }
}

async fn run_deploy_script_slot_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
    slot: DeployScriptSlot,
    scripts: &DeployScriptSet,
) -> Result<Option<ComposeCommandOutput>, AppError> {
    if slot.script_content(scripts).trim().is_empty() {
        return Ok(None);
    }
    context
        .tasks
        .update_phase(context.task_id, slot.phase())
        .await?;
    let step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        slot.step_key(),
        &format!("{} {}", node.name, slot.title()),
        &format!("sh {}", slot.relative_path()),
    )
    .await?;
    let output = run_deploy_script_command_on_node(context, node, slot).await?;
    append_step_command_output(context.tasks, context.task_id, step_id, &output).await?;
    let message = deploy_script_step_message(slot, &output);
    finish_task_step(
        context.tasks,
        context.task_id,
        step_id,
        output.success,
        output.status_code.map(i64::from),
        &message,
    )
    .await?;
    Ok(Some(output))
}

async fn run_deploy_script_command_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
    slot: DeployScriptSlot,
) -> Result<ComposeCommandOutput, AppError> {
    let env = compose_script_environment(context, node);
    let script_path = slot.relative_path();
    match node.node_type.as_str() {
        "local" => context
            .compose
            .run_script(context.runtime_work_dir.to_path_buf(), &script_path, &env)
            .await
            .map_err(AppError::from),
        "ssh" => {
            let target = node.ssh_target()?;
            let remote_work_dir = compose_node_deploy_work_dir(context.job, node);
            context
                .ssh
                .run_script(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    &remote_work_dir,
                    &script_path,
                    &env,
                )
                .await
                .map_err(AppError::from)
        }
        _ => Err(AppError::InvalidInput(format!(
            "节点 {} 的类型 {} 不支持部署脚本",
            node.name, node.node_type
        ))),
    }
}

fn deploy_script_step_message(slot: DeployScriptSlot, output: &ComposeCommandOutput) -> String {
    if output.success {
        format!("{}完成", slot.title())
    } else {
        friendly_command_error(&output.output, &format!("{}失败", slot.title()))
    }
}

fn compose_script_environment(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
) -> Vec<(String, String)> {
    let app_dir = compose_node_runtime_dir_for_script(context, node);
    let release_id = context
        .job
        .release_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let release_version = context.job.release_version.clone().unwrap_or_default();
    let release_dir = if release_version.is_empty() {
        String::new()
    } else {
        compose_script_join(&app_dir, &format!("{RELEASES_DIR_NAME}/{release_version}"))
    };
    let active_slot = "blue";
    let standby = standby_slot(active_slot);
    vec![
        ("ED_APP_ID".to_owned(), context.job.app_id.to_string()),
        ("ED_APP_KEY".to_owned(), context.job.app_key.clone()),
        ("ED_APP_NAME".to_owned(), context.job.app_name.clone()),
        ("ED_ENVIRONMENT".to_owned(), context.job.environment.clone()),
        ("ED_APP_DIR".to_owned(), app_dir.clone()),
        ("ED_RELEASE_ID".to_owned(), release_id),
        ("ED_RELEASE_VERSION".to_owned(), release_version),
        (
            "ED_RELEASE_PACKAGE".to_owned(),
            context.job.release_package_name.clone().unwrap_or_default(),
        ),
        (
            "ED_RELEASE_SHA256".to_owned(),
            context
                .job
                .release_checksum_sha256
                .clone()
                .unwrap_or_default(),
        ),
        (
            "ED_RELEASE_SIZE_BYTES".to_owned(),
            context
                .job
                .release_size_bytes
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        (
            "ED_RELEASE_STORAGE_PROVIDER".to_owned(),
            context
                .job
                .release_storage_provider
                .clone()
                .unwrap_or_default(),
        ),
        (
            "ED_RELEASE_OBJECT_KEY".to_owned(),
            context
                .job
                .release_storage_object_key
                .clone()
                .unwrap_or_default(),
        ),
        ("ED_RELEASE_DIR".to_owned(), release_dir.clone()),
        (
            "ED_RELEASE_BUNDLE_DIR".to_owned(),
            compose_script_join(&release_dir, "bundle"),
        ),
        (
            "ED_RELEASE_RENDER_DIR".to_owned(),
            compose_script_join(&release_dir, "render"),
        ),
        (
            "ED_CURRENT_LINK".to_owned(),
            compose_script_join(&app_dir, CURRENT_RELEASE_FILE_NAME),
        ),
        ("ED_TARGET_NODE_KEY".to_owned(), node.node_key.clone()),
        ("ED_TARGET_NODE_NAME".to_owned(), node.name.clone()),
        (
            "ED_COMPOSE_STRATEGY".to_owned(),
            context.job.compose_strategy.clone(),
        ),
        ("ED_ACTIVE_SLOT".to_owned(), active_slot.to_owned()),
        ("ED_STANDBY_SLOT".to_owned(), standby.to_owned()),
    ]
}

fn compose_node_runtime_dir_for_script(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
) -> String {
    if node.node_type == "ssh" {
        compose_node_deploy_work_dir(context.job, node)
    } else {
        context
            .runtime_work_dir
            .to_string_lossy()
            .replace('\\', "/")
    }
}

fn compose_script_join(root: &str, relative: &str) -> String {
    if root.trim().is_empty() {
        return String::new();
    }
    remote_join(root, relative)
}

fn render_release_package_download_command(
    release_dir: &str,
    package_path: &str,
    signed_url: &str,
    checksum: &str,
    size_bytes: i64,
) -> String {
    format!(
        r#"set -eu
release_dir={release_dir}
package_path={package_path}
mkdir -p "$release_dir" "$release_dir/bundle"
curl -fL --retry 3 --connect-timeout 10 -o "$package_path" {signed_url}
actual_size="$(wc -c < "$package_path" | tr -d ' ')"
test "$actual_size" = "{size_bytes}"
printf '%s  %s\n' {checksum} "$package_path" | sha256sum -c -
case "$package_path" in
  *.tar.gz|*.tgz) tar -xzf "$package_path" -C "$release_dir" ;;
esac"#,
        release_dir = shell_quote_script(release_dir),
        package_path = shell_quote_script(package_path),
        signed_url = shell_quote_script(signed_url),
        checksum = shell_quote_script(checksum),
        size_bytes = size_bytes,
    )
}

fn shell_quote_script(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn run_compose_preflight_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
    step_id: Option<i64>,
) -> Result<ComposeNodeTaskResult, AppError> {
    match node.node_type.as_str() {
        "local" => {
            run_local_compose_preflight(
                context.tasks,
                context.compose,
                context.task_id,
                step_id,
                context.runtime_work_dir.to_path_buf(),
            )
            .await
        }
        "ssh" => run_ssh_compose_preflight(context, node, step_id).await,
        _ => Err(AppError::InvalidInput(format!(
            "节点 {} 的类型 {} 不支持 Compose 部署",
            node.name, node.node_type
        ))),
    }
}

async fn run_local_compose_preflight(
    tasks: &TaskService,
    compose: &ComposeExecutor,
    task_id: i64,
    step_id: Option<i64>,
    work_dir: PathBuf,
) -> Result<ComposeNodeTaskResult, AppError> {
    Ok(run_compose_preflight(tasks, compose, task_id, step_id, work_dir).await)
}

async fn run_ssh_compose_preflight(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
    step_id: Option<i64>,
) -> Result<ComposeNodeTaskResult, AppError> {
    append_task_or_step_log(
        context.tasks,
        context.task_id,
        step_id,
        "system",
        &format!("开始节点 {} Compose 远程预检", node.name),
    )
    .await?;
    let target = node.ssh_target()?;
    let remote_work_dir = compose_node_deploy_work_dir(context.job, node);
    let output = context
        .ssh
        .compose_config(
            &target,
            context.runtime_work_dir.to_path_buf(),
            &remote_work_dir,
        )
        .await?;
    append_intermediate_command_output_for_step(context.tasks, context.task_id, step_id, &output)
        .await?;
    let success = output.success;
    let message = if success {
        "Compose 远程配置校验通过".to_owned()
    } else {
        friendly_command_error(&output.output, "远程 docker compose config 失败")
    };
    Ok(ComposeNodeTaskResult {
        success,
        message,
        outputs: vec![output],
    })
}

async fn run_compose_health_check_on_node(
    context: &ComposeTaskExecutionContext<'_>,
    node: &AppTargetNode,
    step_id: Option<i64>,
) -> Result<ComposeNodeTaskResult, AppError> {
    let config = load_health_check_config(context.db, context.job.app_id).await?;
    if node.node_type == "local" {
        return match run_app_health_check(
            context.db,
            context.tasks,
            context.compose,
            context.systemd,
            context.job.app_id,
            context.task_id,
            step_id,
            context.runtime_work_dir,
        )
        .await?
        {
            true => Ok(ComposeNodeTaskResult {
                success: true,
                message: format!("节点 {} 健康检查通过", node.name),
                outputs: Vec::new(),
            }),
            false => Ok(ComposeNodeTaskResult {
                success: false,
                message: "健康检查失败".to_owned(),
                outputs: Vec::new(),
            }),
        };
    }

    match config.kind {
        HealthCheckKind::None => Ok(ComposeNodeTaskResult {
            success: true,
            message: "未配置健康检查".to_owned(),
            outputs: Vec::new(),
        }),
        HealthCheckKind::ComposeRunning => {
            append_task_or_step_log(
                context.tasks,
                context.task_id,
                step_id,
                "system",
                &format!("开始节点 {} 容器运行状态检查", node.name),
            )
            .await?;
            let target = node.ssh_target()?;
            let remote_work_dir = compose_node_deploy_work_dir(context.job, node);
            let output = context
                .ssh
                .compose_ps_running(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    &remote_work_dir,
                )
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let running_lines = count_compose_running_lines(&output.output);
            let success = output.success && running_lines > 0;
            let message = if success {
                format!("容器运行状态检查通过: {running_lines} 个运行中容器")
            } else {
                friendly_command_error(&output.output, "容器运行状态检查失败: 未发现运行中容器")
            };
            Ok(ComposeNodeTaskResult {
                success,
                message,
                outputs: vec![output],
            })
        }
        HealthCheckKind::Http => {
            let target = node.ssh_target()?;
            let output = context
                .ssh
                .http_health_check(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    &config.endpoint,
                    config.timeout_secs,
                )
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let status = output.output.trim();
            let success = output.success && status == config.expected_status.to_string().as_str();
            let message = if success {
                format!("HTTP 健康检查通过: {status}")
            } else {
                friendly_command_error(
                    &output.output,
                    &format!(
                        "HTTP 健康检查失败: 返回 {status}，期望 {}",
                        config.expected_status
                    ),
                )
            };
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &message)
                .await?;
            Ok(ComposeNodeTaskResult {
                success,
                message,
                outputs: vec![output],
            })
        }
        HealthCheckKind::Tcp => {
            let target = node.ssh_target()?;
            let output = context
                .ssh
                .tcp_health_check(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    &config.endpoint,
                    config.timeout_secs,
                )
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let success = output.success;
            let message = if success {
                format!("TCP 健康检查通过: {}", config.endpoint)
            } else {
                friendly_command_error(
                    &output.output,
                    &format!("TCP 健康检查失败: {}", config.endpoint),
                )
            };
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &message)
                .await?;
            Ok(ComposeNodeTaskResult {
                success,
                message,
                outputs: vec![output],
            })
        }
        _ => Ok(ComposeNodeTaskResult {
            success: true,
            message: format!(
                "SSH 节点 {} 暂跳过 {} 健康检查",
                node.name,
                config.kind.label()
            ),
            outputs: Vec::new(),
        }),
    }
}

async fn sync_compose_runtime_to_ssh_target(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    ssh: &SshExecutor,
    runtime_work_dir: &Path,
    job: &ComposeTaskJob,
    node: &AppTargetNode,
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    let target = node.ssh_target()?;
    let target_root = normalize_remote_target_root(&compose_node_deploy_work_dir(job, node))?;
    let mut files = vec![
        RemoteCopyFile {
            local_path: runtime_work_dir.join("compose.yaml"),
            remote_path: remote_join(&target_root, "compose.yaml"),
        },
        RemoteCopyFile {
            local_path: runtime_work_dir.join(".env"),
            remote_path: remote_join(&target_root, ".env"),
        },
        RemoteCopyFile {
            local_path: runtime_work_dir.join(".easy-deploy").join("app.yaml"),
            remote_path: remote_join(&remote_join(&target_root, ".easy-deploy"), "app.yaml"),
        },
    ];
    let meta_root = runtime_work_dir.join(META_DIR_NAME);
    let legacy_deploy_script = meta_root.join("deploy.sh");
    if legacy_deploy_script.is_file() {
        files.push(RemoteCopyFile {
            local_path: legacy_deploy_script,
            remote_path: remote_join(&remote_join(&target_root, META_DIR_NAME), "deploy.sh"),
        });
    }
    let scripts_dir = meta_root.join("scripts");
    if scripts_dir.is_dir() {
        files.extend(collect_remote_copy_files(
            &scripts_dir,
            &remote_join(&remote_join(&target_root, META_DIR_NAME), "scripts"),
        )?);
    }
    if let Some(release_version) = job.release_version.as_deref() {
        let release_dir = runtime_work_dir
            .join(RELEASES_DIR_NAME)
            .join(release_version);
        files.extend(collect_remote_copy_files(
            &release_dir,
            &remote_join(
                &remote_join(&target_root, RELEASES_DIR_NAME),
                release_version,
            ),
        )?);
    }
    let current_release = runtime_work_dir.join(CURRENT_RELEASE_FILE_NAME);
    if current_release.is_file() {
        files.push(RemoteCopyFile {
            local_path: current_release,
            remote_path: remote_join(&target_root, CURRENT_RELEASE_FILE_NAME),
        });
    }
    let mut outputs = Vec::new();
    for dir in remote_parent_dirs(&files, &target_root) {
        let output = ssh
            .mkdir_all(&target, runtime_work_dir.to_path_buf(), &dir)
            .await?;
        append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await?;
        let success = output.success;
        outputs.push(output);
        if !success {
            return Err(AppError::Internal(format!("SSH 创建目录 {dir} 失败")));
        }
    }
    for file in files {
        let output = ssh
            .copy_file(
                &target,
                runtime_work_dir.to_path_buf(),
                file.local_path,
                &file.remote_path,
            )
            .await?;
        append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await?;
        let success = output.success;
        let remote_path = file.remote_path;
        outputs.push(output);
        if !success {
            return Err(AppError::Internal(format!(
                "SSH 同步文件 {remote_path} 失败"
            )));
        }
    }
    append_task_or_step_log(
        tasks,
        task_id,
        step_id,
        "system",
        &format!(
            "已同步 Compose 运行文件到 SSH 节点 {}: {}",
            node.name, target_root
        ),
    )
    .await?;
    Ok(outputs)
}

async fn sync_current_release_pointer_to_ssh_targets(
    tasks: &TaskService,
    task_id: i64,
    ssh: &SshExecutor,
    runtime_work_dir: &Path,
    job: &ComposeTaskJob,
    nodes: &[AppTargetNode],
) -> Result<(), AppError> {
    let current_file = runtime_work_dir.join(CURRENT_RELEASE_FILE_NAME);
    if !current_file.is_file() {
        return Err(AppError::Internal(format!(
            "当前生效版本指针不存在: {}",
            current_file.to_string_lossy()
        )));
    }
    for node in nodes.iter().filter(|node| node.node_type == "ssh") {
        let target = node.ssh_target()?;
        let target_root = normalize_remote_target_root(&compose_node_deploy_work_dir(job, node))?;
        let remote_path = remote_join(&target_root, CURRENT_RELEASE_FILE_NAME);
        let output = ssh
            .copy_file(
                &target,
                runtime_work_dir.to_path_buf(),
                current_file.clone(),
                &remote_path,
            )
            .await?;
        append_intermediate_command_output_for_step(tasks, task_id, None, &output).await?;
        if !output.success {
            return Err(AppError::Internal(format!(
                "SSH 同步当前生效版本指针到 {} 失败",
                node.name
            )));
        }
    }
    Ok(())
}

fn compose_node_deploy_work_dir(job: &ComposeTaskJob, node: &AppTargetNode) -> String {
    let app = target_work_dir_path(&node.work_dir, &job.app_key);
    if app.starts_with('/') {
        app
    } else {
        node.work_dir.clone()
    }
}

fn compose_node_deploy_work_dir_for_app(app: &AppDetailItem, node: &AppTargetNode) -> String {
    let target = target_work_dir_path(&node.work_dir, &app.app_key);
    if target.starts_with('/') {
        target
    } else {
        node.work_dir.clone()
    }
}

fn count_compose_running_lines(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty()
                && !line.starts_with("NAME")
                && !line.starts_with("time=\"")
                && !line.starts_with("---")
        })
        .count()
}

async fn load_app_compose_strategy(db: &SqlitePool, app_id: i64) -> Result<String, AppError> {
    let strategy = sqlx::query_scalar::<_, String>(
        r#"
        SELECT compose_strategy
        FROM apps
        WHERE id = ?1
        "#,
    )
    .bind(app_id)
    .fetch_optional(db)
    .await?
    .unwrap_or_else(|| "recreate".to_owned());
    let strategy = strategy.trim();
    Ok(match strategy {
        "blue_green" => "blue_green".to_owned(),
        _ => "recreate".to_owned(),
    })
}

#[derive(Debug)]
struct BinaryNodeTaskResult {
    success: bool,
    message: String,
    outputs: Vec<ComposeCommandOutput>,
    promoted_slot: Option<String>,
}

struct BinaryTaskExecutionContext<'a> {
    db: &'a SqlitePool,
    compose: &'a ComposeExecutor,
    systemd: &'a SystemdExecutor,
    ssh: &'a SshExecutor,
    tasks: &'a TaskService,
    task_id: i64,
    runtime_work_dir: &'a Path,
    job: &'a BinaryTaskJob,
}

async fn run_binary_task_on_node(
    context: &BinaryTaskExecutionContext<'_>,
    node: &AppTargetNode,
) -> Result<BinaryNodeTaskResult, AppError> {
    context
        .tasks
        .append_log(
            context.task_id,
            "system",
            &format!("开始在节点 {}({}) 执行二进制任务", node.name, node.node_key),
        )
        .await?;

    context
        .tasks
        .update_phase(context.task_id, "preflight")
        .await?;
    let preflight_step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        "node.preflight",
        &format!("节点 {} 预检", node.name),
        "check node availability and proxy capability",
    )
    .await?;
    if let Some(message) =
        block_unavailable_node_preflight(context.tasks, context.task_id, node).await?
    {
        fail_task_step_message(context.tasks, context.task_id, preflight_step_id, &message).await?;
        return Ok(BinaryNodeTaskResult {
            success: false,
            message,
            outputs: Vec::new(),
            promoted_slot: None,
        });
    }
    if let Some(message) =
        block_missing_proxy_preflight(context.tasks, context.task_id, node, context.job).await?
    {
        fail_task_step_message(context.tasks, context.task_id, preflight_step_id, &message).await?;
        return Ok(BinaryNodeTaskResult {
            success: false,
            message,
            outputs: Vec::new(),
            promoted_slot: None,
        });
    }
    finish_task_step(
        context.tasks,
        context.task_id,
        preflight_step_id,
        true,
        None,
        "节点预检通过",
    )
    .await?;

    let mut outputs = Vec::new();
    if context.job.action.syncs_runtime_files() {
        context
            .tasks
            .update_phase(context.task_id, "preparing_files")
            .await?;
        let prepare_step_id = start_task_step(
            context.tasks,
            context.task_id,
            Some(node),
            "binary.prepare",
            &format!("准备 {} 的二进制运行文件", node.name),
            "sync runtime files && systemctl link && daemon-reload",
        )
        .await?;
        let unit_name = context.job.execution_unit_name();
        let prepare_result: Result<(), AppError> = async {
            match node.node_type.as_str() {
                "local" => {
                    sync_binary_runtime_to_local_target(
                        context.tasks,
                        context.task_id,
                        Some(prepare_step_id),
                        context.runtime_work_dir,
                        context.job,
                        node,
                    )
                    .await?;
                    let command_work_dir = binary_command_work_dir(
                        &binary_node_deploy_work_dir(context.job, node),
                        context.runtime_work_dir,
                    );
                    if let Some(artifact_path) = binary_target_artifact_path(
                        &binary_node_deploy_work_dir(context.job, node),
                        &context.job.artifact_path,
                    ) {
                        let output = context
                            .systemd
                            .make_executable(command_work_dir.clone(), &artifact_path)
                            .await?;
                        append_step_command_output(
                            context.tasks,
                            context.task_id,
                            prepare_step_id,
                            &output,
                        )
                        .await?;
                        let success = output.success;
                        let message = friendly_command_error(&output.output, "chmod +x 版本包失败");
                        outputs.push(output);
                        if !success {
                            Err(AppError::Internal(message))?
                        }
                    }
                    let unit_path = binary_systemd_unit_path(&command_work_dir, &unit_name);
                    let output = context
                        .systemd
                        .link_unit(command_work_dir.clone(), unit_path)
                        .await?;
                    append_step_command_output(
                        context.tasks,
                        context.task_id,
                        prepare_step_id,
                        &output,
                    )
                    .await?;
                    let success = output.success;
                    let message = friendly_command_error(&output.output, "systemctl link failed");
                    outputs.push(output);
                    if !success {
                        Err(AppError::Internal(message))?;
                    }
                    let output = context.systemd.daemon_reload(command_work_dir).await?;
                    append_step_command_output(
                        context.tasks,
                        context.task_id,
                        prepare_step_id,
                        &output,
                    )
                    .await?;
                    let success = output.success;
                    let message =
                        friendly_command_error(&output.output, "systemctl daemon-reload 失败");
                    outputs.push(output);
                    if !success {
                        Err(AppError::Internal(message))?;
                    }
                    Ok(())
                }
                "ssh" => {
                    let sync_outputs = sync_binary_runtime_to_ssh_target(
                        context.tasks,
                        context.task_id,
                        Some(prepare_step_id),
                        context.ssh,
                        context.runtime_work_dir,
                        context.job,
                        node,
                    )
                    .await?;
                    outputs.extend(sync_outputs);
                    let target = node.ssh_target()?;
                    if let Some(remote_artifact_path) = binary_target_artifact_path(
                        &binary_node_deploy_work_dir(context.job, node),
                        &context.job.artifact_path,
                    ) {
                        let output = context
                            .ssh
                            .make_executable(
                                &target,
                                context.runtime_work_dir.to_path_buf(),
                                &remote_artifact_path,
                            )
                            .await?;
                        append_step_command_output(
                            context.tasks,
                            context.task_id,
                            prepare_step_id,
                            &output,
                        )
                        .await?;
                        let success = output.success;
                        let message =
                            friendly_command_error(&output.output, "远程 chmod +x 版本包失败");
                        outputs.push(output);
                        if !success {
                            Err(AppError::Internal(message))?;
                        }
                    }
                    let remote_unit_path = remote_binary_systemd_unit_path(
                        &binary_node_deploy_work_dir(context.job, node),
                        &unit_name,
                    )?;
                    let output = context
                        .ssh
                        .link_unit(
                            &target,
                            context.runtime_work_dir.to_path_buf(),
                            &remote_unit_path,
                        )
                        .await?;
                    append_step_command_output(
                        context.tasks,
                        context.task_id,
                        prepare_step_id,
                        &output,
                    )
                    .await?;
                    let success = output.success;
                    let message =
                        friendly_command_error(&output.output, "remote systemctl link failed");
                    outputs.push(output);
                    if !success {
                        Err(AppError::Internal(message))?;
                    }
                    let output = context
                        .ssh
                        .daemon_reload(&target, context.runtime_work_dir.to_path_buf())
                        .await?;
                    append_step_command_output(
                        context.tasks,
                        context.task_id,
                        prepare_step_id,
                        &output,
                    )
                    .await?;
                    let success = output.success;
                    let message =
                        friendly_command_error(&output.output, "远程 systemctl daemon-reload 失败");
                    outputs.push(output);
                    if !success {
                        Err(AppError::Internal(message))?;
                    }
                    Ok(())
                }
                _ => Err(AppError::InvalidInput(format!(
                    "节点 {} 的类型 {} 不支持二进制部署",
                    node.name, node.node_type
                ))),
            }
        }
        .await;
        if let Err(err) = prepare_result {
            fail_task_step_message(
                context.tasks,
                context.task_id,
                prepare_step_id,
                err.message(),
            )
            .await?;
            return Ok(BinaryNodeTaskResult {
                success: false,
                message: err.message().to_owned(),
                outputs,
                promoted_slot: None,
            });
        }
        finish_task_step(
            context.tasks,
            context.task_id,
            prepare_step_id,
            true,
            Some(0),
            "二进制运行文件准备完成",
        )
        .await?;
    }

    context
        .tasks
        .update_phase(context.task_id, "executing")
        .await?;
    let action_command =
        binary_action_command_label(context.job.action, &context.job.execution_unit_name());
    let action_step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        "binary.action",
        &format!("{} {}", node.name, context.job.action.label()),
        &action_command,
    )
    .await?;
    let output = match node.node_type.as_str() {
        "local" => {
            let command_work_dir = binary_command_work_dir(
                &binary_node_deploy_work_dir(context.job, node),
                context.runtime_work_dir,
            );
            let unit_name = context.job.execution_unit_name();
            match context.job.action {
                BinaryTaskAction::Restart => {
                    context
                        .systemd
                        .restart(command_work_dir, &unit_name)
                        .await?
                }
                BinaryTaskAction::Stop => {
                    context.systemd.stop(command_work_dir, &unit_name).await?
                }
            }
        }
        "ssh" => {
            let target = node.ssh_target()?;
            let unit_name = context.job.execution_unit_name();
            match context.job.action {
                BinaryTaskAction::Restart => {
                    context
                        .ssh
                        .restart(&target, context.runtime_work_dir.to_path_buf(), &unit_name)
                        .await?
                }
                BinaryTaskAction::Stop => {
                    context
                        .ssh
                        .stop(&target, context.runtime_work_dir.to_path_buf(), &unit_name)
                        .await?
                }
            }
        }
        _ => {
            return Err(AppError::InvalidInput(format!(
                "节点 {} 的类型 {} 不支持二进制部署",
                node.name, node.node_type
            )));
        }
    };
    append_step_command_output(context.tasks, context.task_id, action_step_id, &output).await?;
    let command_success = output.success;
    let mut message = friendly_command_error(&output.output, "命令没有输出");
    finish_task_step(
        context.tasks,
        context.task_id,
        action_step_id,
        command_success,
        output.status_code.map(i64::from),
        &message,
    )
    .await?;
    outputs.push(output);

    if command_success && context.job.action.runs_health_check() {
        context
            .tasks
            .update_phase(context.task_id, "healthchecking")
            .await?;
        let health_step_id = start_task_step(
            context.tasks,
            context.task_id,
            Some(node),
            "binary.healthcheck",
            &format!("{} 健康检查", node.name),
            "health check",
        )
        .await?;
        let health = run_binary_health_check_on_node(context, node, Some(health_step_id)).await?;
        message = health.message.clone();
        let health_success = health.success;
        finish_binary_task_step_result(context.tasks, context.task_id, health_step_id, &health)
            .await?;
        outputs.extend(health.outputs);
        return Ok(BinaryNodeTaskResult {
            success: health_success,
            message,
            outputs,
            promoted_slot: context.job.promoted_slot(health_success),
        });
    }

    Ok(BinaryNodeTaskResult {
        success: command_success,
        message,
        outputs,
        promoted_slot: context.job.promoted_slot(command_success),
    })
}

async fn run_binary_health_check_on_node(
    context: &BinaryTaskExecutionContext<'_>,
    node: &AppTargetNode,
    step_id: Option<i64>,
) -> Result<BinaryNodeTaskResult, AppError> {
    let config = load_health_check_config(context.db, context.job.app_id).await?;
    if node.node_type == "local" {
        let command_work_dir = binary_command_work_dir(
            &binary_node_deploy_work_dir(context.job, node),
            context.runtime_work_dir,
        );
        if context.job.release_strategy == "blue_green" {
            return match run_binary_slot_health_check(
                context,
                &config,
                node,
                &command_work_dir,
                None,
                step_id,
            )
            .await?
            {
                true => Ok(BinaryNodeTaskResult {
                    success: true,
                    message: format!(
                        "节点 {} 备用槽位 {} 健康检查通过",
                        node.name,
                        context.job.target_slot()
                    ),
                    outputs: Vec::new(),
                    promoted_slot: None,
                }),
                false => Ok(BinaryNodeTaskResult {
                    success: false,
                    message: "备用槽位健康检查失败，保留当前槽位".to_owned(),
                    outputs: Vec::new(),
                    promoted_slot: None,
                }),
            };
        }
        return match run_app_health_check(
            context.db,
            context.tasks,
            context.compose,
            context.systemd,
            context.job.app_id,
            context.task_id,
            step_id,
            &command_work_dir,
        )
        .await?
        {
            true => Ok(BinaryNodeTaskResult {
                success: true,
                message: format!("节点 {} 健康检查通过", node.name),
                outputs: Vec::new(),
                promoted_slot: None,
            }),
            false => Ok(BinaryNodeTaskResult {
                success: false,
                message: "健康检查失败".to_owned(),
                outputs: Vec::new(),
                promoted_slot: None,
            }),
        };
    }

    match config.kind {
        HealthCheckKind::None => Ok(BinaryNodeTaskResult {
            success: true,
            message: "未配置健康检查".to_owned(),
            outputs: Vec::new(),
            promoted_slot: None,
        }),
        HealthCheckKind::SystemdActive => {
            if context.job.release_strategy == "blue_green" {
                let target = node.ssh_target()?;
                let success = run_binary_slot_health_check(
                    context,
                    &config,
                    node,
                    context.runtime_work_dir,
                    Some(&target),
                    step_id,
                )
                .await?;
                return Ok(BinaryNodeTaskResult {
                    success,
                    message: if success {
                        format!(
                            "节点 {} 备用槽位 {} 健康检查通过",
                            node.name,
                            context.job.target_slot()
                        )
                    } else {
                        "备用槽位健康检查失败，保留当前槽位".to_owned()
                    },
                    outputs: Vec::new(),
                    promoted_slot: None,
                });
            }
            append_task_or_step_log(
                context.tasks,
                context.task_id,
                step_id,
                "system",
                &format!("开始节点 {} systemd active 检查", node.name),
            )
            .await?;
            let target = node.ssh_target()?;
            let endpoint = if context.job.release_strategy == "blue_green" {
                context.job.execution_unit_name()
            } else {
                config.endpoint.clone()
            };
            let output = context
                .ssh
                .is_active(&target, context.runtime_work_dir.to_path_buf(), &endpoint)
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let success = output.success && output.output.trim() == "active";
            let message = if success {
                format!("systemd active 检查通过: {endpoint}")
            } else {
                friendly_command_error(
                    &output.output,
                    &format!("systemd active 检查失败: {endpoint}"),
                )
            };
            Ok(BinaryNodeTaskResult {
                success,
                message,
                outputs: vec![output],
                promoted_slot: None,
            })
        }
        HealthCheckKind::Http => {
            if context.job.release_strategy == "blue_green" {
                let target = node.ssh_target()?;
                let success = run_binary_slot_health_check(
                    context,
                    &config,
                    node,
                    context.runtime_work_dir,
                    Some(&target),
                    step_id,
                )
                .await?;
                return Ok(BinaryNodeTaskResult {
                    success,
                    message: if success {
                        format!(
                            "节点 {} 备用槽位 {} 健康检查通过",
                            node.name,
                            context.job.target_slot()
                        )
                    } else {
                        "备用槽位健康检查失败，保留当前槽位".to_owned()
                    },
                    outputs: Vec::new(),
                    promoted_slot: None,
                });
            }
            let target = node.ssh_target()?;
            let endpoint = config.endpoint.clone();
            let output = context
                .ssh
                .http_health_check(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    &endpoint,
                    config.timeout_secs,
                )
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let status = output.output.trim();
            let success = output.success && status == config.expected_status.to_string().as_str();
            let message = if success {
                format!("HTTP 健康检查通过: {status}")
            } else {
                friendly_command_error(
                    &output.output,
                    &format!(
                        "HTTP 健康检查失败: 返回 {status}，期望 {}",
                        config.expected_status
                    ),
                )
            };
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &message)
                .await?;
            Ok(BinaryNodeTaskResult {
                success,
                message,
                outputs: vec![output],
                promoted_slot: None,
            })
        }
        HealthCheckKind::Tcp => {
            if context.job.release_strategy == "blue_green" {
                let target = node.ssh_target()?;
                let success = run_binary_slot_health_check(
                    context,
                    &config,
                    node,
                    context.runtime_work_dir,
                    Some(&target),
                    step_id,
                )
                .await?;
                return Ok(BinaryNodeTaskResult {
                    success,
                    message: if success {
                        format!(
                            "节点 {} 备用槽位 {} 健康检查通过",
                            node.name,
                            context.job.target_slot()
                        )
                    } else {
                        "备用槽位健康检查失败，保留当前槽位".to_owned()
                    },
                    outputs: Vec::new(),
                    promoted_slot: None,
                });
            }
            let target = node.ssh_target()?;
            let endpoint = config.endpoint.clone();
            let output = context
                .ssh
                .tcp_health_check(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    &endpoint,
                    config.timeout_secs,
                )
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let success = output.success;
            let message = if success {
                format!("TCP 健康检查通过: {}", config.endpoint)
            } else {
                friendly_command_error(
                    &output.output,
                    &format!("TCP 健康检查失败: {}", config.endpoint),
                )
            };
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &message)
                .await?;
            Ok(BinaryNodeTaskResult {
                success,
                message,
                outputs: vec![output],
                promoted_slot: None,
            })
        }
        _ => Ok(BinaryNodeTaskResult {
            success: true,
            message: format!(
                "SSH 节点 {} 暂跳过 {} 健康检查",
                node.name,
                config.kind.label()
            ),
            outputs: Vec::new(),
            promoted_slot: None,
        }),
    }
}

async fn run_binary_slot_health_check(
    context: &BinaryTaskExecutionContext<'_>,
    config: &HealthCheckConfig,
    _node: &AppTargetNode,
    work_dir: &Path,
    ssh_target: Option<&SshTarget>,
    step_id: Option<i64>,
) -> Result<bool, AppError> {
    append_task_or_step_log(
        context.tasks,
        context.task_id,
        step_id,
        "system",
        &format!(
            "开始 Blue/Green 备用槽位 {} 健康检查: {}",
            context.job.target_slot(),
            config.kind.label()
        ),
    )
    .await?;
    match config.kind {
        HealthCheckKind::None => {
            append_task_or_step_log(
                context.tasks,
                context.task_id,
                step_id,
                "system",
                "未配置健康检查",
            )
            .await?;
            Ok(true)
        }
        HealthCheckKind::SystemdActive => {
            let unit_name = context.job.execution_unit_name();
            let output = if let Some(target) = ssh_target {
                context
                    .ssh
                    .is_active(target, context.runtime_work_dir.to_path_buf(), &unit_name)
                    .await?
            } else {
                context
                    .systemd
                    .is_active(work_dir.to_path_buf(), &unit_name)
                    .await?
            };
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                step_id,
                &output,
            )
            .await?;
            let success = output.success && output.output.trim() == "active";
            let message = if success {
                format!("systemd active 检查通过: {unit_name}")
            } else {
                friendly_command_error(
                    &output.output,
                    &format!("systemd active 检查失败: {unit_name}"),
                )
            };
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &message)
                .await?;
            Ok(success)
        }
        HealthCheckKind::Http => {
            let endpoint = context.job.slot_health_endpoint(&config.endpoint);
            if let Some(target) = ssh_target {
                let output = context
                    .ssh
                    .http_health_check(
                        target,
                        context.runtime_work_dir.to_path_buf(),
                        &endpoint,
                        config.timeout_secs,
                    )
                    .await?;
                append_intermediate_command_output_for_step(
                    context.tasks,
                    context.task_id,
                    step_id,
                    &output,
                )
                .await?;
                let status = output.output.trim();
                let success = output.success && status == config.expected_status.to_string();
                let message = if success {
                    format!("HTTP 健康检查通过: {status}")
                } else {
                    friendly_command_error(
                        &output.output,
                        &format!(
                            "HTTP 健康检查失败: 返回 {status}，期望 {}",
                            config.expected_status
                        ),
                    )
                };
                append_task_or_step_log(
                    context.tasks,
                    context.task_id,
                    step_id,
                    "system",
                    &message,
                )
                .await?;
                return Ok(success);
            }
            let outcome =
                run_slot_http_check(&endpoint, config.timeout_secs, config.expected_status).await;
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &outcome)
                .await?;
            Ok(outcome.starts_with("HTTP 健康检查通过"))
        }
        HealthCheckKind::Tcp => {
            let endpoint = context.job.slot_health_endpoint(&config.endpoint);
            if let Some(target) = ssh_target {
                let output = context
                    .ssh
                    .tcp_health_check(
                        target,
                        context.runtime_work_dir.to_path_buf(),
                        &endpoint,
                        config.timeout_secs,
                    )
                    .await?;
                append_intermediate_command_output_for_step(
                    context.tasks,
                    context.task_id,
                    step_id,
                    &output,
                )
                .await?;
                let success = output.success;
                let message = if success {
                    format!("TCP 健康检查通过: {endpoint}")
                } else {
                    friendly_command_error(&output.output, &format!("TCP 健康检查失败: {endpoint}"))
                };
                append_task_or_step_log(
                    context.tasks,
                    context.task_id,
                    step_id,
                    "system",
                    &message,
                )
                .await?;
                return Ok(success);
            }
            let outcome = run_slot_tcp_check(&endpoint, config.timeout_secs).await;
            append_task_or_step_log(context.tasks, context.task_id, step_id, "system", &outcome)
                .await?;
            Ok(outcome.starts_with("TCP 健康检查通过"))
        }
        HealthCheckKind::ComposeRunning => {
            append_task_or_step_log(
                context.tasks,
                context.task_id,
                step_id,
                "system",
                "Blue/Green 二进制部署不使用容器运行状态检查，已跳过",
            )
            .await?;
            Ok(true)
        }
    }
}

async fn run_slot_http_check(endpoint: &str, timeout_secs: u64, expected_status: u16) -> String {
    let Ok(url) = Url::parse(endpoint) else {
        return format!("HTTP 健康检查地址无效: {endpoint}");
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs.clamp(1, 60)))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
    {
        Ok(client) => client,
        Err(err) => return format!("HTTP 健康检查客户端创建失败: {err}"),
    };
    match client.get(url).send().await {
        Ok(response) if response.status().as_u16() == expected_status => {
            format!("HTTP 健康检查通过: {}", response.status().as_u16())
        }
        Ok(response) => format!(
            "HTTP 健康检查失败: 返回 {}，期望 {}",
            response.status().as_u16(),
            expected_status
        ),
        Err(err) => format!("HTTP 健康检查失败: {err}"),
    }
}

async fn run_slot_tcp_check(endpoint: &str, timeout_secs: u64) -> String {
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs.clamp(1, 60)),
        TcpStream::connect(endpoint),
    )
    .await
    {
        Ok(Ok(_)) => format!("TCP 健康检查通过: {endpoint}"),
        Ok(Err(err)) => format!("TCP 健康检查失败: {endpoint}: {err}"),
        Err(_) => format!("TCP 健康检查超时: {endpoint}"),
    }
}

async fn block_unavailable_node_preflight(
    tasks: &TaskService,
    task_id: i64,
    node: &AppTargetNode,
) -> Result<Option<String>, AppError> {
    let message = match node.status.as_str() {
        "offline" => format!("节点 {} 当前离线，预检阻断部署", node.name),
        _ => return Ok(None),
    };
    tasks.append_log(task_id, "system", &message).await?;
    Ok(Some(message))
}

async fn block_missing_proxy_preflight(
    tasks: &TaskService,
    task_id: i64,
    node: &AppTargetNode,
    job: &BinaryTaskJob,
) -> Result<Option<String>, AppError> {
    if job.action != BinaryTaskAction::Restart || !job.proxy_enabled {
        return Ok(None);
    }
    let missing = match job.proxy_kind.as_str() {
        "caddy" if node.caddy_available == 0 => Some("Caddy"),
        "nginx" if node.nginx_available == 0 => Some("Nginx"),
        _ => None,
    };
    let Some(proxy_name) = missing else {
        return Ok(None);
    };
    let message = format!(
        "节点 {} 未通过 {} 能力探测，已启用反向代理切流，预检阻断部署",
        node.name, proxy_name
    );
    tasks.append_log(task_id, "system", &message).await?;
    Ok(Some(message))
}

async fn app_target_nodes(db: &SqlitePool, app_id: i64) -> Result<Vec<AppTargetNode>, AppError> {
    sqlx::query_as::<_, AppTargetNode>(
        r#"
        SELECT
            n.id,
            n.node_key,
            n.name,
            n.node_type,
            n.status,
            n.address,
            n.ssh_port,
            n.ssh_user,
            cred.private_key_path AS credential_private_key_path,
            n.work_dir,
            COALESCE(c.caddy_available, 0) AS caddy_available,
            COALESCE(c.nginx_available, 0) AS nginx_available
        FROM nodes n
        JOIN app_targets t ON t.node_id = n.id
        LEFT JOIN node_credentials cred ON cred.id = n.credential_id
        LEFT JOIN node_capabilities c ON c.node_id = n.id
        WHERE t.app_id = ?1
          AND n.status != 'disabled'
        ORDER BY n.id
        "#,
    )
    .bind(app_id)
    .fetch_all(db)
    .await
    .map_err(AppError::from)
}

fn merge_command_outputs(
    outputs: Vec<ComposeCommandOutput>,
    success: bool,
    fallback_command: &str,
) -> ComposeCommandOutput {
    let command = outputs
        .last()
        .map(|output| output.command.clone())
        .unwrap_or_else(|| fallback_command.to_owned());
    let status_code = if success {
        Some(0)
    } else {
        outputs
            .iter()
            .rev()
            .find_map(|output| output.status_code.filter(|code| *code != 0))
            .or(Some(1))
    };
    let ordered_outputs = if success {
        outputs.iter().collect::<Vec<_>>()
    } else {
        outputs
            .iter()
            .filter(|output| !output.success)
            .chain(outputs.iter().filter(|output| output.success))
            .collect::<Vec<_>>()
    };
    let output = ordered_outputs
        .into_iter()
        .filter_map(|output| {
            let body = output.output.trim();
            if body.is_empty() {
                None
            } else {
                Some(format!("$ {}\n{}", output.command, body))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    ComposeCommandOutput {
        command,
        success,
        status_code,
        output,
    }
}

fn prepend_failure_context(
    mut output: ComposeCommandOutput,
    context: &str,
) -> ComposeCommandOutput {
    if output.success {
        return output;
    }

    let context = context.trim();
    if context.is_empty() || output.output.contains(context) {
        return output;
    }

    output.output = if output.output.trim().is_empty() {
        context.to_owned()
    } else {
        format!("{context}\n{}", output.output)
    };
    output
}

async fn append_intermediate_command_output_for_step(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    output: &ComposeCommandOutput,
) -> Result<(), TaskError> {
    let command_summary = format!(
        "{} · 退出码 {}",
        output.command,
        output
            .status_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "无".to_owned())
    );
    if let Some(step_id) = step_id {
        tasks
            .append_step_log(task_id, step_id, "system", &command_summary)
            .await?;
    } else {
        tasks
            .append_log(task_id, "system", &command_summary)
            .await?;
    }
    if !output.output.trim().is_empty() {
        if let Some(step_id) = step_id {
            tasks
                .append_step_log(task_id, step_id, "combined", &output.output)
                .await?;
        } else {
            tasks
                .append_log(task_id, "combined", &output.output)
                .await?;
        }
    }
    Ok(())
}

async fn start_task_step(
    tasks: &TaskService,
    task_id: i64,
    node: Option<&AppTargetNode>,
    step_key: &str,
    title: &str,
    command: &str,
) -> Result<i64, AppError> {
    tasks
        .start_step(StartTaskStepInput {
            task_id,
            node_id: node.map(|node| node.id),
            step_key,
            title,
            command,
        })
        .await
        .map_err(AppError::from)
}

async fn finish_task_step(
    tasks: &TaskService,
    task_id: i64,
    step_id: i64,
    success: bool,
    exit_code: Option<i64>,
    message: &str,
) -> Result<(), AppError> {
    if success {
        tasks
            .finish_step(task_id, step_id, exit_code, message)
            .await
            .map_err(AppError::from)
    } else {
        tasks
            .fail_step(task_id, step_id, exit_code, message)
            .await
            .map_err(AppError::from)
    }
}

async fn finish_task_step_result(
    tasks: &TaskService,
    task_id: i64,
    step_id: i64,
    result: &ComposeNodeTaskResult,
) -> Result<(), AppError> {
    let exit_code = result
        .outputs
        .iter()
        .rev()
        .find_map(|output| output.status_code.map(i64::from));
    finish_task_step(
        tasks,
        task_id,
        step_id,
        result.success,
        exit_code,
        &result.message,
    )
    .await
}

async fn finish_binary_task_step_result(
    tasks: &TaskService,
    task_id: i64,
    step_id: i64,
    result: &BinaryNodeTaskResult,
) -> Result<(), AppError> {
    let exit_code = result
        .outputs
        .iter()
        .rev()
        .find_map(|output| output.status_code.map(i64::from));
    finish_task_step(
        tasks,
        task_id,
        step_id,
        result.success,
        exit_code,
        &result.message,
    )
    .await
}

async fn fail_task_step_message(
    tasks: &TaskService,
    task_id: i64,
    step_id: i64,
    message: &str,
) -> Result<(), AppError> {
    tasks
        .fail_step(task_id, step_id, None, message)
        .await
        .map_err(AppError::from)
}

async fn append_step_command_output(
    tasks: &TaskService,
    task_id: i64,
    step_id: i64,
    output: &ComposeCommandOutput,
) -> Result<(), AppError> {
    append_intermediate_command_output_for_step(tasks, task_id, Some(step_id), output)
        .await
        .map_err(AppError::from)
}

async fn append_task_or_step_log(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    stream: &str,
    content: &str,
) -> Result<(), TaskError> {
    if let Some(step_id) = step_id {
        tasks
            .append_step_log(task_id, step_id, stream, content)
            .await
    } else {
        tasks.append_log(task_id, stream, content).await
    }
}

fn binary_command_work_dir(deploy_work_dir: &str, runtime_work_dir: &Path) -> PathBuf {
    let target = PathBuf::from(deploy_work_dir);
    if target.is_absolute() {
        target
    } else {
        runtime_work_dir.to_path_buf()
    }
}

fn binary_systemd_unit_path(root: &Path, unit_name: &str) -> PathBuf {
    root.join(META_DIR_NAME)
        .join(SYSTEMD_DIR_NAME)
        .join(unit_name)
}

fn remote_binary_systemd_unit_path(root: &str, unit_name: &str) -> Result<String, AppError> {
    let root = normalize_remote_target_root(root)?;
    Ok(remote_join(
        &remote_join(&remote_join(&root, META_DIR_NAME), SYSTEMD_DIR_NAME),
        unit_name,
    ))
}

async fn sync_binary_runtime_to_local_target(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    runtime_work_dir: &Path,
    job: &BinaryTaskJob,
    node: &AppTargetNode,
) -> Result<(), AppError> {
    let target_root =
        binary_command_work_dir(&binary_node_deploy_work_dir(job, node), runtime_work_dir);
    tokio::fs::create_dir_all(&target_root)
        .await
        .map_err(|err| {
            AppError::Internal(format!(
                "创建二进制目标部署目录 {} 失败: {err}",
                target_root.to_string_lossy()
            ))
        })?;

    let release_source = runtime_work_dir
        .join("releases")
        .join(&job.artifact_version);
    let release_target = target_root.join("releases").join(&job.artifact_version);
    copy_dir_all(&release_source, &release_target)?;

    let systemd_source = runtime_work_dir.join(".easy-deploy").join("systemd");
    let systemd_target = target_root.join(".easy-deploy").join("systemd");
    copy_dir_all(&systemd_source, &systemd_target)?;

    copy_file(
        &runtime_work_dir.join("current"),
        &target_root.join("current"),
        "同步 current 指针",
    )?;
    copy_file(
        &runtime_work_dir.join(".easy-deploy").join("app.yaml"),
        &target_root.join(".easy-deploy").join("app.yaml"),
        "同步 app.yaml",
    )?;
    append_task_or_step_log(
        tasks,
        task_id,
        step_id,
        "system",
        &format!(
            "已同步二进制运行文件到本机部署目录: {}",
            target_root.to_string_lossy()
        ),
    )
    .await?;
    Ok(())
}

async fn sync_binary_runtime_to_ssh_target(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    ssh: &SshExecutor,
    runtime_work_dir: &Path,
    job: &BinaryTaskJob,
    node: &AppTargetNode,
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    let target = node.ssh_target()?;
    let target_root = normalize_remote_target_root(&binary_node_deploy_work_dir(job, node))?;
    let mut outputs = Vec::new();

    let release_source = runtime_work_dir
        .join("releases")
        .join(&job.artifact_version);
    let systemd_source = runtime_work_dir.join(".easy-deploy").join("systemd");
    let mut files = collect_remote_copy_files(
        &release_source,
        &remote_join(
            &remote_join(&target_root, "releases"),
            &job.artifact_version,
        ),
    )?;
    files.extend(collect_remote_copy_files(
        &systemd_source,
        &remote_join(&remote_join(&target_root, ".easy-deploy"), "systemd"),
    )?);
    files.extend([
        RemoteCopyFile {
            local_path: runtime_work_dir.join("current"),
            remote_path: remote_join(&target_root, "current"),
        },
        RemoteCopyFile {
            local_path: runtime_work_dir.join(".easy-deploy").join("app.yaml"),
            remote_path: remote_join(&remote_join(&target_root, ".easy-deploy"), "app.yaml"),
        },
    ]);

    for dir in remote_parent_dirs(&files, &target_root) {
        let output = ssh
            .mkdir_all(&target, runtime_work_dir.to_path_buf(), &dir)
            .await?;
        append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await?;
        let success = output.success;
        outputs.push(output);
        if !success {
            return Err(AppError::Internal(format!("SSH 创建目录 {dir} 失败")));
        }
    }

    for file in files {
        let output = ssh
            .copy_file(
                &target,
                runtime_work_dir.to_path_buf(),
                file.local_path,
                &file.remote_path,
            )
            .await?;
        append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await?;
        let success = output.success;
        let remote_path = file.remote_path;
        outputs.push(output);
        if !success {
            return Err(AppError::Internal(format!(
                "SSH 同步文件 {remote_path} 失败"
            )));
        }
    }

    append_task_or_step_log(
        tasks,
        task_id,
        step_id,
        "system",
        &format!(
            "已同步二进制运行文件到 SSH 节点 {}: {}",
            node.name, target_root
        ),
    )
    .await?;

    Ok(outputs)
}

async fn sync_promoted_binary_runtime_to_targets(
    tasks: &TaskService,
    task_id: i64,
    ssh: &SshExecutor,
    runtime_work_dir: &Path,
    job: &BinaryTaskJob,
    target_nodes: &[AppTargetNode],
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    let mut outputs = Vec::new();
    for node in target_nodes {
        match node.node_type.as_str() {
            "local" => {
                sync_binary_runtime_to_local_target(
                    tasks,
                    task_id,
                    None,
                    runtime_work_dir,
                    job,
                    node,
                )
                .await?;
            }
            "ssh" => {
                outputs.extend(
                    sync_binary_runtime_to_ssh_target(
                        tasks,
                        task_id,
                        None,
                        ssh,
                        runtime_work_dir,
                        job,
                        node,
                    )
                    .await?,
                );
            }
            _ => {
                return Err(AppError::InvalidInput(format!(
                    "节点 {} 的类型 {} 不支持二进制部署",
                    node.name, node.node_type
                )));
            }
        }
    }
    Ok(outputs)
}

async fn switch_binary_proxy_to_targets(
    tasks: &TaskService,
    task_id: i64,
    systemd: &SystemdExecutor,
    ssh: &SshExecutor,
    runtime_work_dir: &Path,
    job: &BinaryTaskJob,
    target_nodes: &[AppTargetNode],
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    if !job.proxy_enabled {
        return Ok(Vec::new());
    }
    if !job.is_blue_green_restart() {
        return Err(AppError::InvalidInput(
            "反向代理切流仅支持 Blue/Green restart 任务".to_owned(),
        ));
    }
    let proxy_config = render_binary_proxy_config(job)?;
    let context = BinaryProxySwitchContext {
        tasks,
        task_id,
        systemd,
        ssh,
        runtime_work_dir,
        job,
        proxy_config: &proxy_config,
    };
    let mut outputs = Vec::new();
    for node in target_nodes {
        outputs.extend(switch_binary_proxy_on_node(&context, node).await?);
    }
    Ok(outputs)
}

async fn cleanup_binary_standby_slot(
    tasks: &TaskService,
    task_id: i64,
    systemd: &SystemdExecutor,
    ssh: &SshExecutor,
    runtime_work_dir: &Path,
    job: &BinaryTaskJob,
    target_nodes: &[AppTargetNode],
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    if !job.is_blue_green_restart() {
        return Ok(Vec::new());
    }
    let unit_name = job.execution_unit_name();
    let mut outputs = Vec::new();
    for node in target_nodes {
        let step_id = start_task_step(
            tasks,
            task_id,
            Some(node),
            "binary.cleanup_standby",
            &format!("停止 {} 的备用槽位", node.name),
            &format!("systemctl stop {unit_name}"),
        )
        .await?;
        let output_result = match node.node_type.as_str() {
            "local" => {
                let command_work_dir = binary_command_work_dir(
                    &binary_node_deploy_work_dir(job, node),
                    runtime_work_dir,
                );
                systemd.stop(command_work_dir, &unit_name).await
            }
            "ssh" => {
                let target = node.ssh_target()?;
                ssh.stop(&target, runtime_work_dir.to_path_buf(), &unit_name)
                    .await
            }
            _ => {
                let message = format!(
                    "节点 {} 的类型 {} 不支持备用槽位清理",
                    node.name, node.node_type
                );
                fail_task_step_message(tasks, task_id, step_id, &message).await?;
                return Err(AppError::InvalidInput(message));
            }
        };
        let output = match output_result {
            Ok(output) => output,
            Err(err) => {
                fail_task_step_message(tasks, task_id, step_id, err.message()).await?;
                return Err(AppError::from(err));
            }
        };
        append_step_command_output(tasks, task_id, step_id, &output).await?;
        let success = output.success;
        let message = friendly_command_error(&output.output, "停止备用槽位失败");
        outputs.push(output);
        if !success {
            fail_task_step_message(tasks, task_id, step_id, &message).await?;
            return Err(AppError::Internal(message));
        }
        finish_task_step(tasks, task_id, step_id, true, Some(0), "备用槽位已停止").await?;
    }
    Ok(outputs)
}

struct BinaryProxySwitchContext<'a> {
    tasks: &'a TaskService,
    task_id: i64,
    systemd: &'a SystemdExecutor,
    ssh: &'a SshExecutor,
    runtime_work_dir: &'a Path,
    job: &'a BinaryTaskJob,
    proxy_config: &'a str,
}

async fn switch_binary_proxy_on_node(
    context: &BinaryProxySwitchContext<'_>,
    node: &AppTargetNode,
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    let job = context.job;
    let config_path = binary_proxy_config_path(context.job)?;
    let step_id = start_task_step(
        context.tasks,
        context.task_id,
        Some(node),
        "binary.proxy_switch",
        &format!("切换 {} 的反向代理", node.name),
        &format!(
            "{} -> {}({})",
            binary_proxy_kind_label(&job.proxy_kind),
            job.target_slot(),
            display_port(job.target_port())
        ),
    )
    .await?;
    append_task_or_step_log(
        context.tasks,
        context.task_id,
        Some(step_id),
        "system",
        &format!(
            "开始在节点 {} 切换 {} 反向代理到 {}({})",
            node.name,
            binary_proxy_kind_label(&job.proxy_kind),
            job.target_slot(),
            display_port(job.target_port())
        ),
    )
    .await?;
    let result = match node.node_type.as_str() {
        "local" => {
            write_local_proxy_config(&config_path, context.proxy_config).await?;
            let command_work_dir = binary_command_work_dir(
                &binary_node_deploy_work_dir(job, node),
                context.runtime_work_dir,
            );
            run_local_proxy_switch(
                context.tasks,
                context.task_id,
                Some(step_id),
                context.systemd,
                command_work_dir,
                &job.proxy_kind,
                &config_path,
            )
            .await
        }
        "ssh" => {
            let target = node.ssh_target()?;
            let local_proxy_path = write_runtime_proxy_config(
                context.runtime_work_dir,
                &node.node_key,
                job,
                context.proxy_config,
            )
            .await?;
            let mut outputs = Vec::new();
            let parent = remote_parent_path(&config_path)?;
            let output = context
                .ssh
                .mkdir_all(&target, context.runtime_work_dir.to_path_buf(), &parent)
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                Some(step_id),
                &output,
            )
            .await?;
            let success = output.success;
            outputs.push(output);
            if !success {
                return Err(AppError::Internal(format!(
                    "SSH 创建代理配置目录 {parent} 失败"
                )));
            }

            let output = context
                .ssh
                .copy_file(
                    &target,
                    context.runtime_work_dir.to_path_buf(),
                    local_proxy_path,
                    &config_path,
                )
                .await?;
            append_intermediate_command_output_for_step(
                context.tasks,
                context.task_id,
                Some(step_id),
                &output,
            )
            .await?;
            let success = output.success;
            outputs.push(output);
            if !success {
                return Err(AppError::Internal(format!(
                    "SSH 同步代理配置 {config_path} 失败"
                )));
            }

            outputs.extend(
                run_ssh_proxy_switch(
                    context.tasks,
                    context.task_id,
                    Some(step_id),
                    context.ssh,
                    &target,
                    context.runtime_work_dir,
                    &job.proxy_kind,
                    &config_path,
                )
                .await?,
            );
            Ok(outputs)
        }
        _ => Err(AppError::InvalidInput(format!(
            "节点 {} 的类型 {} 不支持反向代理切流",
            node.name, node.node_type
        ))),
    };
    match result {
        Ok(outputs) => {
            let exit_code = outputs
                .iter()
                .rev()
                .find_map(|output| output.status_code.map(i64::from))
                .or(Some(0));
            finish_task_step(
                context.tasks,
                context.task_id,
                step_id,
                true,
                exit_code,
                "反向代理切流完成",
            )
            .await?;
            Ok(outputs)
        }
        Err(err) => {
            fail_task_step_message(context.tasks, context.task_id, step_id, err.message()).await?;
            Err(err)
        }
    }
}

async fn run_local_proxy_switch(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    systemd: &SystemdExecutor,
    work_dir: PathBuf,
    proxy_kind: &str,
    config_path: &str,
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    let mut outputs = Vec::new();
    let validate = match proxy_kind {
        "nginx" => {
            systemd
                .nginx_validate(work_dir.clone(), config_path)
                .await?
        }
        _ => {
            systemd
                .caddy_validate(work_dir.clone(), config_path)
                .await?
        }
    };
    append_intermediate_command_output_for_step(tasks, task_id, step_id, &validate).await?;
    let success = validate.success;
    let message = friendly_command_error(&validate.output, "反向代理配置校验失败");
    outputs.push(validate);
    if !success {
        return Err(AppError::Internal(message));
    }

    let service_name = proxy_systemd_service_name(proxy_kind);
    let reload = systemd.reload_service(work_dir, service_name).await?;
    append_intermediate_command_output_for_step(tasks, task_id, step_id, &reload).await?;
    let success = reload.success;
    let message = friendly_command_error(&reload.output, "反向代理 reload 失败");
    outputs.push(reload);
    if !success {
        return Err(AppError::Internal(message));
    }
    Ok(outputs)
}

#[allow(clippy::too_many_arguments)]
async fn run_ssh_proxy_switch(
    tasks: &TaskService,
    task_id: i64,
    step_id: Option<i64>,
    ssh: &SshExecutor,
    target: &SshTarget,
    runtime_work_dir: &Path,
    proxy_kind: &str,
    config_path: &str,
) -> Result<Vec<ComposeCommandOutput>, AppError> {
    let mut outputs = Vec::new();
    let validate = match proxy_kind {
        "nginx" => {
            ssh.nginx_validate(target, runtime_work_dir.to_path_buf(), config_path)
                .await?
        }
        _ => {
            ssh.caddy_validate(target, runtime_work_dir.to_path_buf(), config_path)
                .await?
        }
    };
    append_intermediate_command_output_for_step(tasks, task_id, step_id, &validate).await?;
    let success = validate.success;
    let message = friendly_command_error(&validate.output, "远程反向代理配置校验失败");
    outputs.push(validate);
    if !success {
        return Err(AppError::Internal(message));
    }

    let service_name = proxy_systemd_service_name(proxy_kind);
    let reload = ssh
        .reload_service(target, runtime_work_dir.to_path_buf(), service_name)
        .await?;
    append_intermediate_command_output_for_step(tasks, task_id, step_id, &reload).await?;
    let success = reload.success;
    let message = friendly_command_error(&reload.output, "远程反向代理 reload 失败");
    outputs.push(reload);
    if !success {
        return Err(AppError::Internal(message));
    }
    Ok(outputs)
}

async fn write_local_proxy_config(path: &str, content: &str) -> Result<(), AppError> {
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            AppError::Internal(format!(
                "创建反向代理配置目录 {} 失败: {err}",
                parent.display()
            ))
        })?;
    }
    tokio::fs::write(&path, content).await.map_err(|err| {
        AppError::Internal(format!("写入反向代理配置 {} 失败: {err}", path.display()))
    })
}

async fn write_runtime_proxy_config(
    runtime_work_dir: &Path,
    node_key: &str,
    job: &BinaryTaskJob,
    content: &str,
) -> Result<PathBuf, AppError> {
    let local_path = runtime_work_dir
        .join(".easy-deploy")
        .join("proxy")
        .join(node_key)
        .join(proxy_config_file_name(job)?);
    if let Some(parent) = local_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|err| {
            AppError::Internal(format!("创建代理临时目录 {} 失败: {err}", parent.display()))
        })?;
    }
    tokio::fs::write(&local_path, content)
        .await
        .map_err(|err| {
            AppError::Internal(format!(
                "写入代理临时配置 {} 失败: {err}",
                local_path.display()
            ))
        })?;
    Ok(local_path)
}

fn render_binary_proxy_config(job: &BinaryTaskJob) -> Result<String, AppError> {
    let domain = required_text(&job.proxy_domain, "请输入反向代理域名")?;
    let port = job.target_port();
    if port <= 0 {
        return Err(AppError::InvalidInput(
            "Blue/Green 切流需要配置目标槽位端口".to_owned(),
        ));
    }
    match job.proxy_kind.as_str() {
        "caddy" => Ok(format!(
            "{domain} {{\n    reverse_proxy 127.0.0.1:{port}\n}}\n"
        )),
        "nginx" => Ok(format!(
            "server {{\n    listen 80;\n    server_name {domain};\n\n    location / {{\n        proxy_set_header Host $host;\n        proxy_set_header X-Real-IP $remote_addr;\n        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n        proxy_set_header X-Forwarded-Proto $scheme;\n        proxy_pass http://127.0.0.1:{port};\n    }}\n}}\n"
        )),
        _ => Err(AppError::InvalidInput("反向代理类型不支持".to_owned())),
    }
}

fn binary_proxy_config_path(job: &BinaryTaskJob) -> Result<String, AppError> {
    let path = job.proxy_config_path.trim();
    if path.is_empty() {
        Ok(default_proxy_config_path(&job.proxy_kind, &job.app_key))
    } else {
        normalize_proxy_config_path(path)
    }
}

fn proxy_config_file_name(job: &BinaryTaskJob) -> Result<String, AppError> {
    let suffix = match job.proxy_kind.as_str() {
        "nginx" => "conf",
        "caddy" => "caddy",
        _ => return Err(AppError::InvalidInput("反向代理类型不支持".to_owned())),
    };
    Ok(format!("{}.{}", job.app_key, suffix))
}

fn remote_parent_path(path: &str) -> Result<String, AppError> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent.to_owned())
        .filter(|parent| !parent.is_empty())
        .ok_or_else(|| AppError::InvalidInput("反向代理配置路径必须包含目录".to_owned()))
}

fn proxy_systemd_service_name(proxy_kind: &str) -> &'static str {
    match proxy_kind {
        "nginx" => "nginx.service",
        _ => "caddy.service",
    }
}

fn binary_proxy_kind_label(proxy_kind: &str) -> &'static str {
    match proxy_kind {
        "nginx" => "Nginx",
        "caddy" => "Caddy",
        _ => "反向代理",
    }
}

#[derive(Debug)]
struct RemoteCopyFile {
    local_path: PathBuf,
    remote_path: String,
}

fn collect_remote_copy_files(
    source: &Path,
    remote_root: &str,
) -> Result<Vec<RemoteCopyFile>, AppError> {
    let mut files = Vec::new();
    collect_remote_copy_files_inner(source, source, remote_root, &mut files)?;
    Ok(files)
}

fn collect_remote_copy_files_inner(
    base: &Path,
    source: &Path,
    remote_root: &str,
    files: &mut Vec<RemoteCopyFile>,
) -> Result<(), AppError> {
    if !source.is_dir() {
        return Err(AppError::InvalidInput(format!(
            "源目录不存在: {}",
            source.to_string_lossy()
        )));
    }
    for entry in fs::read_dir(source).map_err(|err| {
        AppError::Internal(format!("读取目录 {} 失败: {err}", source.to_string_lossy()))
    })? {
        let entry = entry.map_err(|err| AppError::Internal(format!("读取目录条目失败: {err}")))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| {
            AppError::Internal(format!(
                "读取文件类型 {} 失败: {err}",
                path.to_string_lossy()
            ))
        })?;
        if file_type.is_dir() {
            collect_remote_copy_files_inner(base, &path, remote_root, files)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(base).map_err(|err| {
                AppError::Internal(format!(
                    "计算相对路径 {} 失败: {err}",
                    path.to_string_lossy()
                ))
            })?;
            let remote_path = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().to_string())
                .fold(remote_root.to_owned(), |acc, part| remote_join(&acc, &part));
            files.push(RemoteCopyFile {
                local_path: path,
                remote_path,
            });
        }
    }
    Ok(())
}

fn remote_parent_dirs(files: &[RemoteCopyFile], target_root: &str) -> Vec<String> {
    let mut dirs = vec![target_root.to_owned()];
    for file in files {
        if let Some((dir, _)) = file.remote_path.rsplit_once('/')
            && !dir.is_empty()
            && !dirs.iter().any(|existing| existing == dir)
        {
            dirs.push(dir.to_owned());
        }
    }
    dirs
}

fn normalize_remote_target_root(value: &str) -> Result<String, AppError> {
    let value = value.trim().replace('\\', "/");
    if !value.starts_with('/') {
        return Err(AppError::InvalidInput(
            "SSH 二进制部署目录必须是绝对路径".to_owned(),
        ));
    }
    if value.contains("//")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '@'))
        || value.split('/').any(|part| part == "." || part == "..")
    {
        return Err(AppError::InvalidInput(
            "SSH 二进制部署目录仅支持字母、数字、斜线、点、短横线、下划线和 @".to_owned(),
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn binary_node_deploy_work_dir(job: &BinaryTaskJob, node: &AppTargetNode) -> String {
    if job.deploy_work_dir.trim().is_empty() {
        target_work_dir_path(&node.work_dir, &job.app_key)
    } else {
        job.deploy_work_dir.clone()
    }
}

fn binary_node_deploy_work_dir_for_app(app: &AppDetailItem, node: &AppTargetNode) -> String {
    if app.work_dir.trim().is_empty() {
        target_work_dir_path(&node.work_dir, &app.app_key)
    } else {
        app.work_dir.clone()
    }
}

fn binary_target_artifact_path(deploy_work_dir: &str, artifact_path: &str) -> Option<String> {
    let deploy_work_dir = deploy_work_dir.trim().replace('\\', "/");
    let artifact_path = artifact_path.trim().replace('\\', "/");
    if deploy_work_dir.is_empty() || artifact_path.is_empty() {
        return None;
    }
    let release_prefix = format!("{}/releases/", deploy_work_dir.trim_end_matches('/'));
    if artifact_path.starts_with(&release_prefix) {
        Some(artifact_path)
    } else {
        None
    }
}

fn remote_join(root: &str, relative: &str) -> String {
    format!(
        "{}/{}",
        root.trim_end_matches('/'),
        relative.trim_matches('/')
    )
}

fn copy_dir_all(source: &Path, target: &Path) -> Result<(), AppError> {
    if !source.is_dir() {
        return Err(AppError::InvalidInput(format!(
            "源目录不存在: {}",
            source.to_string_lossy()
        )));
    }
    fs::create_dir_all(target).map_err(|err| {
        AppError::Internal(format!("创建目录 {} 失败: {err}", target.to_string_lossy()))
    })?;
    for entry in fs::read_dir(source).map_err(|err| {
        AppError::Internal(format!("读取目录 {} 失败: {err}", source.to_string_lossy()))
    })? {
        let entry = entry.map_err(|err| AppError::Internal(format!("读取目录条目失败: {err}")))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type().map_err(|err| {
            AppError::Internal(format!(
                "读取文件类型 {} 失败: {err}",
                source_path.to_string_lossy()
            ))
        })?;
        if file_type.is_dir() {
            copy_dir_all(&source_path, &target_path)?;
        } else if file_type.is_file() {
            copy_file(&source_path, &target_path, "同步文件")?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path, action: &str) -> Result<(), AppError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            AppError::Internal(format!("创建目录 {} 失败: {err}", parent.to_string_lossy()))
        })?;
    }
    fs::copy(source, target).map_err(|err| {
        AppError::Internal(format!(
            "{action} {} -> {} 失败: {err}",
            source.to_string_lossy(),
            target.to_string_lossy()
        ))
    })?;
    Ok(())
}

async fn load_health_check_config(
    db: &SqlitePool,
    app_id: i64,
) -> Result<HealthCheckConfig, AppError> {
    #[derive(sqlx::FromRow)]
    struct HealthCheckRow {
        check_kind: String,
        endpoint: String,
        timeout_secs: i64,
        expected_status: i64,
    }

    let Some(row) = sqlx::query_as::<_, HealthCheckRow>(
        r#"
        SELECT check_kind, endpoint, timeout_secs, expected_status
        FROM app_health_checks
        WHERE app_id = ?1
        "#,
    )
    .bind(app_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(HealthCheckConfig::default());
    };
    normalize_health_config(
        &row.check_kind,
        &row.endpoint,
        row.timeout_secs,
        row.expected_status,
    )
    .map_err(AppError::from)
}

#[allow(clippy::too_many_arguments)]
async fn run_app_health_check(
    db: &SqlitePool,
    tasks: &TaskService,
    compose: &ComposeExecutor,
    systemd: &SystemdExecutor,
    app_id: i64,
    task_id: i64,
    step_id: Option<i64>,
    work_dir: &Path,
) -> Result<bool, AppError> {
    let config = load_health_check_config(db, app_id).await?;
    if let Err(err) = append_task_or_step_log(
        tasks,
        task_id,
        step_id,
        "system",
        &format!("开始健康检查: {}", config.kind.label()),
    )
    .await
    {
        return Err(AppError::from(err));
    }
    let outcome = run_health_check(&config, compose, systemd, work_dir.to_path_buf()).await?;
    if let Err(err) =
        append_task_or_step_log(tasks, task_id, step_id, "system", &outcome.message).await
    {
        return Err(AppError::from(err));
    }
    Ok(outcome.healthy)
}

async fn run_compose_preflight(
    tasks: &TaskService,
    compose: &ComposeExecutor,
    task_id: i64,
    step_id: Option<i64>,
    work_dir: std::path::PathBuf,
) -> ComposeNodeTaskResult {
    let mut outputs = Vec::new();
    if let Err(err) = append_task_or_step_log(
        tasks,
        task_id,
        step_id,
        "system",
        "开始部署前预检: Docker daemon",
    )
    .await
    {
        error!(
            task_id,
            error = %err,
            "failed to append compose preflight log"
        );
        return ComposeNodeTaskResult {
            success: false,
            message: "写入预检日志失败".to_owned(),
            outputs,
        };
    }
    match compose.docker_info(work_dir.clone()).await {
        Ok(output) if output.success => {
            if let Err(err) =
                append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await
            {
                error!(
                    task_id,
                    error = %err,
                    "failed to append docker info output"
                );
                return ComposeNodeTaskResult {
                    success: false,
                    message: "写入 Docker 预检输出失败".to_owned(),
                    outputs,
                };
            }
            outputs.push(output);
            if let Err(err) =
                append_task_or_step_log(tasks, task_id, step_id, "system", "Docker daemon 连接正常")
                    .await
            {
                error!(
                    task_id,
                    error = %err,
                    "failed to append compose preflight log"
                );
                return ComposeNodeTaskResult {
                    success: false,
                    message: "写入预检日志失败".to_owned(),
                    outputs,
                };
            }
        }
        Ok(output) => {
            if let Err(err) =
                append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await
            {
                error!(
                    task_id,
                    error = %err,
                    "failed to append docker info output"
                );
            }
            let message = format!(
                "Docker daemon 预检失败: {}",
                friendly_command_error(&output.output, "docker info 返回非 0 状态")
            );
            outputs.push(output);
            return ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            };
        }
        Err(err) => {
            let message = format!("Docker daemon 预检失败: {}", err.message());
            return ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            };
        }
    }

    match run_local_preflight(&work_dir) {
        Ok(result) => {
            for message in result.messages {
                if let Err(err) =
                    append_task_or_step_log(tasks, task_id, step_id, "system", &message).await
                {
                    error!(
                        task_id,
                        error = %err,
                        "failed to append local preflight log"
                    );
                    return ComposeNodeTaskResult {
                        success: false,
                        message: "写入本地预检日志失败".to_owned(),
                        outputs,
                    };
                }
            }
        }
        Err(err) => {
            let message = format!("本地环境预检失败: {}", err.message());
            return ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            };
        }
    }

    if let Err(err) = append_task_or_step_log(
        tasks,
        task_id,
        step_id,
        "system",
        "开始部署前预检: docker compose config",
    )
    .await
    {
        error!(
            task_id,
            error = %err,
            "failed to append compose config preflight log"
        );
        return ComposeNodeTaskResult {
            success: false,
            message: "写入 Compose 预检日志失败".to_owned(),
            outputs,
        };
    }
    match compose.config(work_dir).await {
        Ok(output) if output.success => {
            if let Err(err) =
                append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await
            {
                error!(
                    task_id,
                    error = %err,
                    "failed to append compose config output"
                );
                return ComposeNodeTaskResult {
                    success: false,
                    message: "写入 Compose 配置输出失败".to_owned(),
                    outputs,
                };
            }
            outputs.push(output);
            if let Err(err) =
                append_task_or_step_log(tasks, task_id, step_id, "system", "Compose 配置校验通过")
                    .await
            {
                error!(
                    task_id,
                    error = %err,
                    "failed to append compose config preflight log"
                );
                return ComposeNodeTaskResult {
                    success: false,
                    message: "写入 Compose 预检日志失败".to_owned(),
                    outputs,
                };
            }
            ComposeNodeTaskResult {
                success: true,
                message: "部署前预检通过".to_owned(),
                outputs,
            }
        }
        Ok(output) => {
            if let Err(err) =
                append_intermediate_command_output_for_step(tasks, task_id, step_id, &output).await
            {
                error!(
                    task_id,
                    error = %err,
                    "failed to append compose config output"
                );
            }
            let message = format!(
                "Compose 配置预检失败: {}",
                friendly_command_error(&output.output, "docker compose config 返回非 0 状态")
            );
            outputs.push(output);
            ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            }
        }
        Err(err) => {
            let message = format!("Compose 配置预检失败: {}", err.message());
            ComposeNodeTaskResult {
                success: false,
                message,
                outputs,
            }
        }
    }
}

struct LocalPreflightResult {
    messages: Vec<String>,
}

fn run_local_preflight(work_dir: &Path) -> Result<LocalPreflightResult, AppError> {
    ensure_directory_writable(work_dir)?;
    let free_space = available_space(work_dir).map_err(|err| {
        AppError::Internal(format!(
            "读取磁盘可用空间失败: {}: {err}",
            work_dir.to_string_lossy()
        ))
    })?;
    if free_space < MIN_COMPOSE_FREE_SPACE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "磁盘可用空间不足，当前 {}，至少需要 {}",
            human_bytes(free_space),
            human_bytes(MIN_COMPOSE_FREE_SPACE_BYTES)
        )));
    }

    let compose_path = work_dir.join("compose.yaml");
    let compose_content = fs::read_to_string(&compose_path).map_err(|err| {
        AppError::Internal(format!(
            "读取 Compose 配置失败: {}: {err}",
            compose_path.to_string_lossy()
        ))
    })?;
    validate_compose_deploy_conventions(&compose_content)?;
    let ports = parse_compose_host_ports(&compose_content)?;
    let occupied_ports = occupied_ports(&ports);

    let mut messages = vec![
        "工作目录可写".to_owned(),
        format!("磁盘可用空间 {}", human_bytes(free_space)),
    ];
    if ports.is_empty() {
        messages.push("Compose 未声明主机端口映射".to_owned());
    } else if occupied_ports.is_empty() {
        messages.push(format!("主机端口未被占用: {}", join_ports(&ports)));
    } else {
        messages.push(format!(
            "主机端口可能已被占用: {}。如果这是当前应用已有容器占用，Compose 会自行重建；否则部署可能失败。",
            join_ports(&occupied_ports)
        ));
    }
    Ok(LocalPreflightResult { messages })
}

fn ensure_directory_writable(work_dir: &Path) -> Result<(), AppError> {
    let probe_path = work_dir.join(".easy-deploy-write-test");
    File::create(&probe_path)
        .and_then(|file| file.sync_all())
        .map_err(|err| {
            AppError::InvalidInput(format!(
                "工作目录不可写: {}: {err}",
                work_dir.to_string_lossy()
            ))
        })?;
    fs::remove_file(&probe_path).map_err(|err| {
        AppError::InvalidInput(format!(
            "工作目录写入探针清理失败: {}: {err}",
            probe_path.to_string_lossy()
        ))
    })?;
    Ok(())
}

fn parse_compose_host_ports(compose_content: &str) -> Result<Vec<u16>, AppError> {
    let value = serde_yaml::from_str::<Value>(compose_content)
        .map_err(|err| AppError::InvalidInput(format!("Compose YAML 解析失败: {err}")))?;
    let mut ports = Vec::new();
    let Some(services) = value.get("services").and_then(Value::as_mapping) else {
        return Ok(ports);
    };
    for service in services.values() {
        let Some(port_entries) = service.get("ports").and_then(Value::as_sequence) else {
            continue;
        };
        for entry in port_entries {
            match entry {
                Value::Number(number) => {
                    if let Some(port) = number.as_u64().and_then(to_port) {
                        ports.push(port);
                    }
                }
                Value::String(value) => {
                    if let Some(port) = parse_compose_port_string(value) {
                        ports.push(port);
                    }
                }
                Value::Mapping(mapping) => {
                    if let Some(port) = mapping
                        .get(Value::String("published".to_owned()))
                        .and_then(value_to_port)
                    {
                        ports.push(port);
                    }
                }
                _ => {}
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    Ok(ports)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedService {
    name: String,
    image: String,
    ports: String,
    replicas: String,
}

fn parse_compose_services(compose_content: &str) -> Result<Vec<ParsedService>, AppError> {
    if compose_content.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value = serde_yaml::from_str::<Value>(compose_content)
        .map_err(|err| AppError::InvalidInput(format!("Compose YAML 解析失败: {err}")))?;
    let Some(services) = value.get("services").and_then(Value::as_mapping) else {
        return Ok(Vec::new());
    };
    let mut parsed = Vec::new();
    for (name, service) in services {
        let Some(name) = name.as_str() else {
            continue;
        };
        let image = service
            .get("image")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("未配置镜像")
            .to_owned();
        parsed.push(ParsedService {
            name: name.to_owned(),
            image,
            ports: parse_service_ports(service),
            replicas: parse_service_replicas(service),
        });
    }
    parsed.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(parsed)
}

fn parse_service_ports(service: &Value) -> String {
    let Some(entries) = service.get("ports").and_then(Value::as_sequence) else {
        return "未声明端口".to_owned();
    };
    let ports = entries
        .iter()
        .filter_map(render_compose_port)
        .collect::<Vec<_>>();
    if ports.is_empty() {
        "未声明端口".to_owned()
    } else {
        ports.join(", ")
    }
}

fn render_compose_port(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => number.as_u64().map(|port| port.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Mapping(mapping) => {
            let target = mapping
                .get(Value::String("target".to_owned()))
                .and_then(value_to_port);
            let published = mapping
                .get(Value::String("published".to_owned()))
                .and_then(value_to_port);
            match (published, target) {
                (Some(published), Some(target)) => Some(format!("{published}:{target}")),
                (None, Some(target)) => Some(target.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_service_replicas(service: &Value) -> String {
    service
        .get("deploy")
        .and_then(|deploy| deploy.get("replicas"))
        .and_then(Value::as_i64)
        .filter(|replicas| *replicas > 0)
        .map(|replicas| replicas.to_string())
        .unwrap_or_else(|| "1".to_owned())
}

fn value_to_port(value: &Value) -> Option<u16> {
    match value {
        Value::Number(number) => number.as_u64().and_then(to_port),
        Value::String(value) => parse_port(value),
        _ => None,
    }
}

fn parse_compose_port_string(value: &str) -> Option<u16> {
    let first_part = value.split('/').next().unwrap_or(value);
    if !first_part.contains(':') {
        return None;
    }
    let host_part = first_part.rsplit(':').nth(1).unwrap_or(first_part);
    let port_part = host_part.split('-').next().unwrap_or(host_part);
    parse_port(port_part)
}

fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

fn to_port(value: u64) -> Option<u16> {
    u16::try_from(value).ok().filter(|port| *port > 0)
}

fn occupied_ports(ports: &[u16]) -> Vec<u16> {
    ports
        .iter()
        .copied()
        .filter(|port| TcpListener::bind(("127.0.0.1", *port)).is_err())
        .collect()
}

fn join_ports(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn human_bytes(value: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if value >= GIB {
        format!("{:.1} GiB", value as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", value as f64 / MIB as f64)
    }
}

fn friendly_command_error(value: &str, fallback: &str) -> String {
    let lines = value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("time=\""))
        .map(strip_common_error_prefix)
        .take(3)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        fallback.to_owned()
    } else {
        lines.join("；")
    }
}

async fn record_deploy_config_snapshot(
    db: &SqlitePool,
    app_id: i64,
    work_dir: &Path,
    source: &str,
    version: &str,
    artifact_version: &str,
    binary: Option<&BinaryConfigItem>,
) -> Result<RuntimeConfigSnapshotRecord, AppError> {
    let compose_content = fs::read_to_string(work_dir.join("compose.yaml")).unwrap_or_default();
    let env_content = fs::read_to_string(work_dir.join(".env")).unwrap_or_default();
    let deploy_scripts = deploy_scripts_from_runtime_dir(work_dir);
    let mut tx = db.begin().await?;
    let snapshot = insert_runtime_config_snapshot(
        &mut tx,
        RuntimeConfigSnapshotInput {
            app_id,
            snapshot_kind: "deploy",
            compose_content: &compose_content,
            env_content: &env_content,
            artifact_version,
            metadata: runtime_snapshot_metadata(
                source,
                work_dir.to_string_lossy(),
                Some(version),
                Some(&deploy_scripts),
                binary,
            ),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(snapshot)
}

async fn bind_deployment_run_snapshot(
    db: &SqlitePool,
    app_id: i64,
    task_id: i64,
    snapshot: &RuntimeConfigSnapshotRecord,
    artifact_version: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        UPDATE deployment_runs
        SET config_snapshot_id = ?3,
            config_revision_no = ?4,
            artifact_version = ?5
        WHERE app_id = ?1
          AND task_id = ?2
        "#,
    )
    .bind(app_id)
    .bind(task_id)
    .bind(snapshot.id)
    .bind(snapshot.revision_no)
    .bind(artifact_version)
    .execute(db)
    .await?;
    Ok(())
}

async fn promote_binary_active_slot(
    db: &SqlitePool,
    runtime_fs: &RuntimeFs,
    job: &BinaryTaskJob,
    work_dir: &Path,
    slot: &str,
) -> Result<(), AppError> {
    update_binary_active_slot(db, job.app_id, slot).await?;

    let app = fetch_app_detail_by_id(db, job.app_id).await?;
    let mut binary_config = fetch_binary_config_for_app(db, job.app_id).await?;
    binary_config.active_slot = slot.to_owned();
    let target_nodes = target_node_metadata_for_app(db, job.app_id).await?;
    let metadata_content = render_runtime_metadata(
        &app,
        target_nodes,
        &work_dir.to_string_lossy(),
        Some(&binary_config),
    );
    runtime_fs
        .save_app_runtime_files(
            &app.app_key,
            "",
            &binary_config.env_content,
            &metadata_content,
        )
        .await?;
    runtime_fs
        .save_binary_runtime_files(to_binary_runtime_config(
            app.id,
            &app.app_key,
            &app.name,
            &binary_config,
        ))
        .await?;
    Ok(())
}

async fn update_binary_active_slot(
    db: &SqlitePool,
    app_id: i64,
    slot: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        UPDATE app_binary_configs
        SET active_slot = ?2,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE app_id = ?1
        "#,
    )
    .bind(app_id)
    .bind(slot)
    .execute(db)
    .await?;
    Ok(())
}

async fn fetch_app_detail_by_id(db: &SqlitePool, app_id: i64) -> Result<AppDetailItem, AppError> {
    sqlx::query_as::<_, AppDetailItem>(
        r#"
        SELECT
            a.id,
            a.app_key,
            a.name,
            a.description,
            a.environment,
            a.app_type,
            a.deploy_mode,
            a.deploy_strategy,
            a.release_source,
            a.compose_strategy,
            a.auto_queue_release,
            a.work_dir,
            a.status,
            GROUP_CONCAT(n.name, '、') AS target_names,
            COUNT(t.node_id) AS target_count,
            a.created_at,
            a.updated_at
        FROM apps a
        LEFT JOIN app_targets t ON t.app_id = a.id
        LEFT JOIN nodes n ON n.id = t.node_id
        WHERE a.id = ?1
        GROUP BY a.id
        "#,
    )
    .bind(app_id)
    .fetch_one(db)
    .await
    .map_err(AppError::from)
}

async fn fetch_binary_config_for_app(
    db: &SqlitePool,
    app_id: i64,
) -> Result<BinaryConfigItem, AppError> {
    sqlx::query_as::<_, BinaryConfigItem>(
        r#"
        SELECT
            service_name,
            artifact_version,
            artifact_path,
            exec_args,
            working_dir,
            service_user,
            unit_name,
            release_strategy,
            active_slot,
            base_port,
            standby_port,
            proxy_enabled,
            proxy_kind,
            proxy_domain,
            proxy_config_path,
            env_content
        FROM app_binary_configs
        WHERE app_id = ?1
        "#,
    )
    .bind(app_id)
    .fetch_one(db)
    .await
    .map_err(AppError::from)
}

async fn target_node_metadata_for_app(
    db: &SqlitePool,
    app_id: i64,
) -> Result<Vec<TargetNodeMetadata>, AppError> {
    #[derive(sqlx::FromRow)]
    struct TargetNodeMetadataRow {
        node_key: String,
        name: String,
    }

    sqlx::query_as::<_, TargetNodeMetadataRow>(
        r#"
        SELECT
            n.node_key,
            n.name
        FROM nodes n
        JOIN app_targets t ON t.node_id = n.id
        WHERE t.app_id = ?1
        ORDER BY n.id
        "#,
    )
    .bind(app_id)
    .fetch_all(db)
    .await
    .map(|nodes| {
        nodes
            .into_iter()
            .map(|node| TargetNodeMetadata {
                node_key: node.node_key,
                name: node.name,
            })
            .collect()
    })
    .map_err(AppError::from)
}

struct RuntimeStatesUpdate<'a> {
    db: &'a SqlitePool,
    app_id: i64,
    runtime_status: &'a str,
    service_count: Option<i64>,
    active_version: Option<&'a str>,
    message: &'a str,
    task_id: Option<i64>,
    touch_deploy_time: bool,
}

async fn update_runtime_states_best_effort(update: RuntimeStatesUpdate<'_>) {
    if let Err(err) = update_runtime_states_in_db(&update).await {
        warn!(
            app_id = update.app_id,
            runtime_status = update.runtime_status,
            error = %err,
            "failed to update app runtime states"
        );
    }
}

struct RuntimeStateUpdate<'a> {
    db: &'a SqlitePool,
    app_id: i64,
    node_id: i64,
    runtime_status: &'a str,
    service_count: Option<i64>,
    active_version: Option<&'a str>,
    message: &'a str,
    task_id: Option<i64>,
    touch_deploy_time: bool,
}

async fn update_runtime_state_for_node_best_effort(update: RuntimeStateUpdate<'_>) {
    if let Err(err) = update_runtime_state_for_node_in_db(&update).await {
        warn!(
            app_id = update.app_id,
            node_id = update.node_id,
            runtime_status = update.runtime_status,
            error = %err,
            "failed to update node runtime state"
        );
    }
}

async fn record_task_node_result_best_effort(
    tasks: &TaskService,
    task_id: i64,
    node: &AppTargetNode,
    status: &str,
    message: &str,
    command_count: usize,
) {
    if let Err(err) = tasks
        .record_node_result(TaskNodeResultInput {
            task_id,
            node_id: node.id,
            node_name: &node.name,
            node_key: &node.node_key,
            node_type: &node.node_type,
            status,
            message,
            command_count: command_count as i64,
        })
        .await
    {
        warn!(
            task_id,
            node_id = node.id,
            status,
            error = %err,
            "failed to record task node result"
        );
    }
}

async fn mark_unexecuted_nodes_after_failure(
    tasks: &TaskService,
    task_id: i64,
    db: &SqlitePool,
    app_id: i64,
    target_nodes: &[AppTargetNode],
    stop_after_node_id: Option<i64>,
    node_messages: &mut Vec<String>,
) {
    let Some(failed_node_id) = stop_after_node_id else {
        return;
    };
    let mut mark_remaining = false;
    for node in target_nodes {
        if mark_remaining {
            let message = "前序节点失败，未执行本次任务";
            record_task_node_result_best_effort(tasks, task_id, node, "skipped", message, 0).await;
            node_messages.push(format!("{}: {message}", node.name));
            update_runtime_state_for_node_best_effort(RuntimeStateUpdate {
                db,
                app_id,
                node_id: node.id,
                runtime_status: "unknown",
                service_count: None,
                active_version: None,
                message,
                task_id: Some(task_id),
                touch_deploy_time: false,
            })
            .await;
        }
        if node.id == failed_node_id {
            mark_remaining = true;
        }
    }
}

async fn update_runtime_states_in_db(update: &RuntimeStatesUpdate<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE app_runtime_states
        SET runtime_status = ?2,
            service_count = COALESCE(?3, service_count),
            active_version = COALESCE(?4, active_version),
            message = ?5,
            last_task_id = COALESCE(?6, last_task_id),
            last_deploy_at = CASE
                WHEN ?7 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                ELSE last_deploy_at
            END,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE app_id = ?1
        "#,
    )
    .bind(update.app_id)
    .bind(update.runtime_status)
    .bind(update.service_count)
    .bind(update.active_version)
    .bind(update.message)
    .bind(update.task_id)
    .bind(update.touch_deploy_time)
    .execute(update.db)
    .await?;
    Ok(())
}

async fn update_runtime_state_for_node_in_db(
    update: &RuntimeStateUpdate<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE app_runtime_states
        SET runtime_status = ?3,
            service_count = COALESCE(?4, service_count),
            active_version = COALESCE(?5, active_version),
            message = ?6,
            last_task_id = COALESCE(?7, last_task_id),
            last_deploy_at = CASE
                WHEN ?8 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                ELSE last_deploy_at
            END,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE app_id = ?1
          AND node_id = ?2
        "#,
    )
    .bind(update.app_id)
    .bind(update.node_id)
    .bind(update.runtime_status)
    .bind(update.service_count)
    .bind(update.active_version)
    .bind(update.message)
    .bind(update.task_id)
    .bind(update.touch_deploy_time)
    .execute(update.db)
    .await?;
    Ok(())
}

async fn insert_runtime_config_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    input: RuntimeConfigSnapshotInput<'_>,
) -> Result<RuntimeConfigSnapshotRecord, sqlx::Error> {
    let revision_no = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(MAX(revision_no), 0) + 1
        FROM app_config_snapshots
        WHERE app_id = ?1
        "#,
    )
    .bind(input.app_id)
    .fetch_one(&mut **tx)
    .await?;
    let config_hash = runtime_config_hash(
        input.compose_content,
        input.env_content,
        input.artifact_version,
        &input.metadata,
    );
    let id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO app_config_snapshots(
            app_id,
            revision_no,
            snapshot_kind,
            compose_content,
            env_content,
            artifact_version,
            config_hash,
            metadata
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        RETURNING id
        "#,
    )
    .bind(input.app_id)
    .bind(revision_no)
    .bind(input.snapshot_kind)
    .bind(input.compose_content)
    .bind(input.env_content)
    .bind(input.artifact_version)
    .bind(config_hash)
    .bind(input.metadata)
    .fetch_one(&mut **tx)
    .await?;

    Ok(RuntimeConfigSnapshotRecord { id, revision_no })
}

fn runtime_config_hash(
    compose_content: &str,
    env_content: &str,
    artifact_version: &str,
    metadata: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(compose_content.as_bytes());
    hasher.update(b"\0");
    hasher.update(env_content.as_bytes());
    hasher.update(b"\0");
    hasher.update(artifact_version.as_bytes());
    hasher.update(b"\0");
    hasher.update(metadata.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn runtime_snapshot_metadata(
    source: &str,
    runtime_root: impl AsRef<str>,
    version: Option<&str>,
    deploy_scripts: Option<&DeployScriptSet>,
    binary: Option<&BinaryConfigItem>,
) -> String {
    let mut metadata = json!({
        "source": source,
        "runtime": "file",
        "runtime_root": runtime_root.as_ref(),
    });
    if let Some(version) = version {
        metadata["version"] = json!(version);
    }
    if let Some(deploy_scripts) = deploy_scripts {
        metadata["deploy_scripts"] = json!(deploy_scripts);
    }
    if let Some(binary) = binary {
        metadata["binary"] = json!({
            "service_name": binary.service_name.as_str(),
            "artifact_version": binary.artifact_version.as_str(),
            "artifact_path": binary.artifact_path.as_str(),
            "exec_args": binary.exec_args.as_str(),
            "working_dir": binary.working_dir.as_str(),
            "service_user": binary.service_user.as_str(),
            "unit_name": binary.unit_name.as_str(),
            "release_strategy": binary.release_strategy.as_str(),
            "active_slot": binary.active_slot.as_str(),
            "base_port": binary.base_port,
            "standby_port": binary.standby_port,
            "proxy_enabled": binary.proxy_enabled,
            "proxy_kind": binary.proxy_kind.as_str(),
            "proxy_domain": binary.proxy_domain.as_str(),
            "proxy_config_path": binary.proxy_config_path.as_str(),
        });
    }
    metadata.to_string()
}

fn snapshot_artifact_version(snapshot: &AppConfigSnapshotItem) -> String {
    if !snapshot.artifact_version.trim().is_empty() {
        return snapshot.artifact_version.trim().to_owned();
    }
    serde_json::from_str::<JsonValue>(&snapshot.metadata)
        .ok()
        .and_then(|metadata| {
            json_string(metadata.get("binary"), "artifact_version")
                .or_else(|| json_string(Some(&metadata), "version"))
        })
        .unwrap_or_default()
}

fn binary_config_from_snapshot(
    app: &AppDetailItem,
    snapshot: &AppConfigSnapshotItem,
    current: &BinaryConfigItem,
    artifact: Option<&BinaryArtifactItem>,
) -> BinaryConfigItem {
    let metadata = serde_json::from_str::<JsonValue>(&snapshot.metadata).ok();
    let binary = metadata
        .as_ref()
        .and_then(|value| value.get("binary"))
        .filter(|value| value.is_object());
    let mut config = current.clone();

    config.service_name =
        json_string(binary, "service_name").unwrap_or_else(|| current.service_name.clone());
    config.exec_args =
        json_string(binary, "exec_args").unwrap_or_else(|| current.exec_args.clone());
    config.working_dir =
        json_string(binary, "working_dir").unwrap_or_else(|| current.working_dir.clone());
    config.service_user =
        json_string(binary, "service_user").unwrap_or_else(|| current.service_user.clone());
    config.unit_name =
        json_string(binary, "unit_name").unwrap_or_else(|| current.unit_name.clone());
    config.release_strategy =
        json_string(binary, "release_strategy").unwrap_or_else(|| current.release_strategy.clone());
    config.active_slot =
        json_string(binary, "active_slot").unwrap_or_else(|| current.active_slot.clone());
    config.base_port = json_i64(binary, "base_port").unwrap_or(current.base_port);
    config.standby_port = json_i64(binary, "standby_port").unwrap_or(current.standby_port);
    config.proxy_enabled = json_i64(binary, "proxy_enabled")
        .or_else(|| json_bool(binary, "proxy_enabled").map(i64::from))
        .unwrap_or(current.proxy_enabled);
    config.proxy_kind =
        json_string(binary, "proxy_kind").unwrap_or_else(|| current.proxy_kind.clone());
    config.proxy_domain =
        json_string(binary, "proxy_domain").unwrap_or_else(|| current.proxy_domain.clone());
    config.proxy_config_path = json_string(binary, "proxy_config_path")
        .unwrap_or_else(|| current.proxy_config_path.clone());
    config.env_content = normalize_env_content(&snapshot.env_content);

    if let Some(artifact) = artifact {
        config.artifact_version = artifact.version.clone();
        config.artifact_path = artifact.artifact_path.clone();
    } else if let Some(version) = json_string(binary, "artifact_version")
        .or_else(|| json_string(metadata.as_ref(), "version"))
        .filter(|value| !value.trim().is_empty())
    {
        config.artifact_version = version;
        if let Some(path) =
            json_string(binary, "artifact_path").filter(|value| !value.trim().is_empty())
        {
            config.artifact_path = path;
        }
    }

    if config.service_name.trim().is_empty() {
        config.service_name = app.app_key.clone();
    }
    if config.working_dir.trim().is_empty() {
        config.working_dir = app.work_dir.clone();
    }
    if config.service_user.trim().is_empty() {
        config.service_user = "deploy".to_owned();
    }
    if config.unit_name.trim().is_empty() {
        config.unit_name = format!("easy-deploy-{}.service", app.app_key);
    }
    if config.release_strategy.trim().is_empty() {
        config.release_strategy = "restart".to_owned();
    }
    if config.active_slot.trim().is_empty() {
        config.active_slot = "blue".to_owned();
    }
    config
}

fn json_string(value: Option<&JsonValue>, key: &str) -> Option<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn json_i64(value: Option<&JsonValue>, key: &str) -> Option<i64> {
    value
        .and_then(|value| value.get(key))
        .and_then(JsonValue::as_i64)
}

fn json_bool(value: Option<&JsonValue>, key: &str) -> Option<bool> {
    value
        .and_then(|value| value.get(key))
        .and_then(JsonValue::as_bool)
}

async fn upsert_binary_config(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    app_id: i64,
    config: &BinaryConfigItem,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO app_binary_configs(
            app_id,
            service_name,
            artifact_version,
            artifact_path,
            exec_args,
            working_dir,
            service_user,
            unit_name,
            release_strategy,
            active_slot,
            base_port,
            standby_port,
            proxy_enabled,
            proxy_kind,
            proxy_domain,
            proxy_config_path,
            env_content,
            updated_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        ON CONFLICT(app_id) DO UPDATE SET
            service_name = excluded.service_name,
            artifact_version = excluded.artifact_version,
            artifact_path = excluded.artifact_path,
            exec_args = excluded.exec_args,
            working_dir = excluded.working_dir,
            service_user = excluded.service_user,
            unit_name = excluded.unit_name,
            release_strategy = excluded.release_strategy,
            active_slot = excluded.active_slot,
            base_port = excluded.base_port,
            standby_port = excluded.standby_port,
            proxy_enabled = excluded.proxy_enabled,
            proxy_kind = excluded.proxy_kind,
            proxy_domain = excluded.proxy_domain,
            proxy_config_path = excluded.proxy_config_path,
            env_content = excluded.env_content,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        "#,
    )
    .bind(app_id)
    .bind(&config.service_name)
    .bind(&config.artifact_version)
    .bind(&config.artifact_path)
    .bind(&config.exec_args)
    .bind(&config.working_dir)
    .bind(&config.service_user)
    .bind(&config.unit_name)
    .bind(&config.release_strategy)
    .bind(&config.active_slot)
    .bind(config.base_port)
    .bind(config.standby_port)
    .bind(config.proxy_enabled)
    .bind(&config.proxy_kind)
    .bind(&config.proxy_domain)
    .bind(&config.proxy_config_path)
    .bind(&config.env_content)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO binary_artifacts(
            app_id,
            version,
            artifact_path,
            artifact_kind,
            status,
            metadata
        )
        VALUES (?1, ?2, ?3, 'binary', 'registered', ?4)
        ON CONFLICT(app_id, version) DO UPDATE SET
            artifact_path = excluded.artifact_path,
            status = 'registered',
            metadata = excluded.metadata
        "#,
    )
    .bind(app_id)
    .bind(&config.artifact_version)
    .bind(&config.artifact_path)
    .bind(format!(
        "{{\"source\":\"manual\",\"unit_name\":\"{}\"}}",
        json_escape(&config.unit_name)
    ))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn prune_uploaded_binary_releases(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    app_id: i64,
    active_version: &str,
    releases_to_keep: usize,
) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query_as::<_, BinaryArtifactItem>(
        r#"
        SELECT
            id,
            version,
            version_code,
            artifact_path,
            artifact_kind,
            status,
            metadata,
            published_at,
            created_at
        FROM binary_artifacts
        WHERE app_id = ?1
          AND status != 'disabled'
        ORDER BY version_code DESC, published_at DESC, id DESC
        "#,
    )
    .bind(app_id)
    .fetch_all(&mut **tx)
    .await?;

    let (prune_ids, pruned_versions) =
        select_uploaded_binary_release_prunes(&rows, active_version, releases_to_keep);

    for artifact_id in prune_ids {
        sqlx::query(
            r#"
            UPDATE binary_artifacts
            SET status = 'disabled'
            WHERE id = ?1
            "#,
        )
        .bind(artifact_id)
        .execute(&mut **tx)
        .await?;
    }

    Ok(pruned_versions)
}

fn select_uploaded_binary_release_prunes(
    rows: &[BinaryArtifactItem],
    active_version: &str,
    releases_to_keep: usize,
) -> (Vec<i64>, Vec<String>) {
    let releases_to_keep = releases_to_keep.max(1);
    let mut seen_uploads = 0usize;
    let mut prune_ids = Vec::new();
    let mut pruned_versions = Vec::new();
    for row in rows {
        if row.metadata_value("source") != "upload" {
            continue;
        }
        seen_uploads += 1;
        if row.version == active_version {
            continue;
        }
        if seen_uploads > releases_to_keep {
            prune_ids.push(row.id);
            pruned_versions.push(row.version.clone());
        }
    }
    (prune_ids, pruned_versions)
}

fn cleanup_pruned_binary_release_dirs(
    runtime_root: &Path,
    versions: &[String],
) -> Result<(), AppError> {
    for version in versions {
        let release_dir = runtime_root.join("releases").join(version);
        match fs::remove_dir_all(&release_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(AppError::Internal(format!(
                    "清理旧二进制版本目录 {} 失败: {err}",
                    release_dir.to_string_lossy()
                )));
            }
        }
    }
    Ok(())
}

fn runtime_service_count(work_dir: &Path) -> i64 {
    let compose_path = work_dir.join("compose.yaml");
    match fs::read_to_string(compose_path) {
        Ok(content) => parse_compose_services(&content)
            .map(|services| services.len() as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

fn build_deploy_diff(
    app: &AppDetailItem,
    compose_content: &str,
    env_content: &str,
    binary_config: &BinaryConfigItem,
    baseline: Option<&AppConfigSnapshotItem>,
) -> AppDeployDiff {
    let Some(baseline) = baseline else {
        return AppDeployDiff {
            baseline_snapshot_id: None,
            baseline_created_at: None,
            status: AppDeployDiffStatus::NoBaseline,
            rows: Vec::new(),
        };
    };

    let mut rows = Vec::new();
    rows.push(diff_row(
        "Compose",
        compose_content,
        &baseline.compose_content,
        summarize_config_content,
    ));
    rows.push(diff_row(
        "环境变量",
        env_content,
        &baseline.env_content,
        summarize_config_content,
    ));

    if app.app_type == "binary" {
        let baseline_binary = binary_config_from_metadata(&baseline.metadata);
        rows.push(diff_row(
            "发布版本",
            &binary_config.artifact_version,
            &baseline_binary.artifact_version,
            summarize_inline_value,
        ));
        rows.push(diff_row(
            "部署文件路径",
            &binary_config.artifact_path,
            &baseline_binary.artifact_path,
            summarize_inline_value,
        ));
        rows.push(diff_row(
            "启动参数",
            &binary_config.exec_args,
            &baseline_binary.exec_args,
            summarize_inline_value,
        ));
        rows.push(diff_row(
            "运行用户",
            &binary_config.service_user,
            &baseline_binary.service_user,
            summarize_inline_value,
        ));
        rows.push(diff_row(
            "Unit 名称",
            &binary_config.unit_name,
            &baseline_binary.unit_name,
            summarize_inline_value,
        ));
    }

    let changed = rows.iter().any(|row| row.changed);
    AppDeployDiff {
        baseline_snapshot_id: Some(baseline.id),
        baseline_created_at: Some(baseline.created_at.clone()),
        status: if changed {
            AppDeployDiffStatus::Changed
        } else {
            AppDeployDiffStatus::Unchanged
        },
        rows,
    }
}

fn diff_row(
    label: &'static str,
    current: &str,
    baseline: &str,
    summarize: fn(&str) -> String,
) -> AppDeployDiffRow {
    let normalized_current = normalize_diff_text(current);
    let normalized_baseline = normalize_diff_text(baseline);
    AppDeployDiffRow {
        label,
        current_summary: summarize(&normalized_current),
        baseline_summary: summarize(&normalized_baseline),
        current_preview: diff_preview(&normalized_current),
        baseline_preview: diff_preview(&normalized_baseline),
        changed: normalized_current != normalized_baseline,
    }
}

fn normalize_diff_text(value: &str) -> String {
    value.trim().replace("\r\n", "\n")
}

fn summarize_config_content(value: &str) -> String {
    if value.trim().is_empty() {
        return "空".to_owned();
    }
    let line_count = value.lines().filter(|line| !line.trim().is_empty()).count();
    let first_line = value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.ends_with(':'))
        .or_else(|| value.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("空")
        .chars()
        .take(80)
        .collect::<String>();
    format!("{line_count} 行 · {first_line}")
}

fn summarize_inline_value(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        "空".to_owned()
    } else {
        value.chars().take(80).collect()
    }
}

fn diff_preview(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return "空".to_owned();
    }
    let mut output = value
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(1200)
        .collect::<String>();
    let truncated_by_lines = value.lines().count() > 12;
    let truncated_by_chars = value.chars().count() > output.chars().count();
    if truncated_by_lines || truncated_by_chars {
        output.push_str("\n...");
    }
    output
}

fn binary_config_from_metadata(metadata: &str) -> BinaryConfigItem {
    let Ok(value) = serde_yaml::from_str::<Value>(metadata) else {
        return BinaryConfigItem::default();
    };
    let Some(binary) = value.get("binary") else {
        return BinaryConfigItem::default();
    };
    BinaryConfigItem {
        service_name: yaml_string(binary, "service_name"),
        artifact_version: yaml_string(binary, "artifact_version"),
        artifact_path: yaml_string(binary, "artifact_path"),
        exec_args: yaml_string(binary, "exec_args"),
        working_dir: yaml_string(binary, "working_dir"),
        service_user: yaml_string(binary, "service_user"),
        unit_name: yaml_string(binary, "unit_name"),
        release_strategy: yaml_string(binary, "release_strategy"),
        active_slot: yaml_string(binary, "active_slot"),
        base_port: yaml_i64(binary, "base_port"),
        standby_port: yaml_i64(binary, "standby_port"),
        proxy_enabled: if yaml_bool(binary, "proxy_enabled") {
            1
        } else {
            0
        },
        proxy_kind: yaml_string(binary, "proxy_kind"),
        proxy_domain: yaml_string(binary, "proxy_domain"),
        proxy_config_path: yaml_string(binary, "proxy_config_path"),
        env_content: String::new(),
    }
}

fn deploy_scripts_from_snapshot_metadata(metadata: &str) -> DeployScriptSet {
    serde_json::from_str::<JsonValue>(metadata)
        .ok()
        .and_then(|value| value.get("deploy_scripts").cloned())
        .and_then(|value| serde_json::from_value::<DeployScriptSet>(value).ok())
        .unwrap_or_default()
}

fn deploy_scripts_from_runtime_dir(work_dir: &Path) -> DeployScriptSet {
    let scripts_dir = work_dir.join(META_DIR_NAME).join("scripts");
    DeployScriptSet {
        pre_deploy: fs::read_to_string(scripts_dir.join("pre_deploy.sh")).unwrap_or_default(),
        deploy: fs::read_to_string(scripts_dir.join("deploy.sh")).unwrap_or_default(),
        post_deploy: fs::read_to_string(scripts_dir.join("post_deploy.sh")).unwrap_or_default(),
        switch_traffic: fs::read_to_string(scripts_dir.join("switch_traffic.sh"))
            .unwrap_or_default(),
        cleanup: fs::read_to_string(scripts_dir.join("cleanup.sh")).unwrap_or_default(),
    }
}

fn yaml_string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn yaml_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or_default()
}

fn yaml_bool(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or_default()
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppListItem {
    pub id: i64,
    pub app_key: String,
    pub name: String,
    pub description: String,
    pub environment: String,
    pub app_type: String,
    pub deploy_mode: String,
    pub deploy_strategy: String,
    pub release_source: String,
    pub compose_strategy: String,
    pub auto_queue_release: i64,
    pub work_dir: String,
    pub status: String,
    pub runtime_status: String,
    pub runtime_summary: String,
    pub target_names: Option<String>,
    pub target_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppDetailItem {
    pub id: i64,
    pub app_key: String,
    pub name: String,
    pub description: String,
    pub environment: String,
    pub app_type: String,
    pub deploy_mode: String,
    pub deploy_strategy: String,
    pub release_source: String,
    pub compose_strategy: String,
    pub auto_queue_release: i64,
    pub work_dir: String,
    pub status: String,
    pub target_names: Option<String>,
    pub target_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub struct AppStatusChange {
    pub app_id: i64,
    pub app_name: String,
    pub previous_status: String,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct AppConfigDetail {
    pub app: AppDetailItem,
    pub runtime_root: String,
    pub compose_content: String,
    pub env_content: String,
    pub deploy_scripts: DeployScriptSet,
    pub metadata_content: String,
    pub service_names: Vec<String>,
    pub binary_runtime: BinaryRuntimeFiles,
    pub health_check: HealthCheckConfig,
    pub deployment_runs: Vec<AppDeploymentRunItem>,
    pub config_snapshots: Vec<AppConfigSnapshotItem>,
    pub deploy_diff: AppDeployDiff,
    pub runtime_states: Vec<AppRuntimeStateItem>,
    pub target_nodes: Vec<AppTargetSummaryItem>,
    pub target_choices: Vec<AppTargetChoiceItem>,
    pub binary_config: BinaryConfigItem,
    pub binary_releases: Vec<BinaryArtifactItem>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppDeploymentRunItem {
    pub id: i64,
    pub task_id: Option<i64>,
    pub task_title: Option<String>,
    pub deploy_action: String,
    pub status: String,
    pub message: String,
    pub config_snapshot_id: Option<i64>,
    pub config_revision_no: i64,
    pub artifact_version: String,
    pub started_at: String,
    pub finished_at: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppReleaseItem {
    pub id: i64,
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub version: String,
    pub version_code: i64,
    pub package_name: String,
    pub package_path: String,
    pub extract_dir: String,
    pub status: String,
    pub source: String,
    pub checksum_sha256: String,
    pub size_bytes: i64,
    pub published_at: String,
    pub received_at: String,
    pub scheduled_publish_at: Option<String>,
    pub storage_provider: String,
    pub storage_bucket: String,
    pub storage_object_key: String,
    pub storage_endpoint: String,
    pub metadata: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppReleaseQueueItem {
    pub id: i64,
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub release_id: i64,
    pub version: String,
    pub version_code: i64,
    pub config_snapshot_id: Option<i64>,
    pub queue_seq: i64,
    pub status: String,
    pub triggered_by: String,
    pub message: String,
    pub task_id: Option<i64>,
    pub scheduled_publish_at: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct PendingReleaseQueueItem {
    id: i64,
    release_id: i64,
    config_snapshot_id: Option<i64>,
    version: String,
    version_code: i64,
    package_name: String,
    package_path: String,
    checksum_sha256: String,
    size_bytes: i64,
    published_at: String,
    storage_provider: String,
    storage_bucket: String,
    storage_object_key: String,
    storage_endpoint: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct AppReleaseUploadRecord {
    id: String,
    app_id: i64,
    release_version: String,
    version_code: i64,
    file_name: String,
    object_key: String,
    bucket: String,
    endpoint: String,
    status: String,
    source: String,
    published_at: String,
    expires_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppConfigSnapshotItem {
    pub id: i64,
    pub revision_no: i64,
    pub snapshot_kind: String,
    pub compose_content: String,
    pub env_content: String,
    pub artifact_version: String,
    pub config_hash: String,
    pub metadata: String,
    pub created_at: String,
}

#[derive(Clone, Debug)]
struct RuntimeConfigSnapshotInput<'a> {
    app_id: i64,
    snapshot_kind: &'a str,
    compose_content: &'a str,
    env_content: &'a str,
    artifact_version: &'a str,
    metadata: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct RuntimeConfigSnapshotRecord {
    id: i64,
    revision_no: i64,
}

#[derive(Clone, Debug)]
pub struct AppDeployDiff {
    pub baseline_snapshot_id: Option<i64>,
    pub baseline_created_at: Option<String>,
    pub status: AppDeployDiffStatus,
    pub rows: Vec<AppDeployDiffRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppDeployDiffStatus {
    NoBaseline,
    Unchanged,
    Changed,
}

#[derive(Clone, Debug)]
pub struct AppDeployDiffRow {
    pub label: &'static str,
    pub current_summary: String,
    pub baseline_summary: String,
    pub current_preview: String,
    pub baseline_preview: String,
    pub changed: bool,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppRuntimeStateItem {
    pub node_id: i64,
    pub node_name: String,
    pub node_key: String,
    pub runtime_status: String,
    pub active_version: String,
    pub service_count: i64,
    pub message: String,
    pub last_task_id: Option<i64>,
    pub last_task_status: Option<String>,
    pub last_task_kind: Option<String>,
    pub last_deploy_at: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppTargetChoiceItem {
    pub id: i64,
    pub name: String,
    pub node_key: String,
    pub checked: bool,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppTargetSummaryItem {
    pub id: i64,
    pub name: String,
    pub node_key: String,
    pub node_type: String,
    pub status: String,
    pub docker_status: String,
    pub capability_status: String,
    pub docker_available: i64,
    pub compose_available: i64,
    pub systemd_available: i64,
    pub caddy_available: i64,
    pub nginx_available: i64,
    pub capability_message: String,
}

#[derive(Clone, Debug, Default, sqlx::FromRow)]
pub struct BinaryConfigItem {
    pub service_name: String,
    pub artifact_version: String,
    pub artifact_path: String,
    pub exec_args: String,
    pub working_dir: String,
    pub service_user: String,
    pub unit_name: String,
    pub release_strategy: String,
    pub active_slot: String,
    pub base_port: i64,
    pub standby_port: i64,
    pub proxy_enabled: i64,
    pub proxy_kind: String,
    pub proxy_domain: String,
    pub proxy_config_path: String,
    pub env_content: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct BinaryArtifactItem {
    pub id: i64,
    pub version: String,
    pub version_code: i64,
    pub artifact_path: String,
    pub artifact_kind: String,
    pub status: String,
    pub metadata: String,
    pub published_at: String,
    pub created_at: String,
}

impl BinaryArtifactItem {
    pub fn metadata_value(&self, key: &str) -> String {
        artifact_metadata_value(&self.metadata, key)
    }
}

pub struct BinaryReleaseDeployResult {
    pub task_id: i64,
    pub version: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AppNodeOption {
    pub id: i64,
    pub name: String,
    pub node_key: String,
}

#[derive(Clone, Debug)]
pub struct ServiceListItem {
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub service_name: String,
    pub service_kind: String,
    pub image: String,
    pub ports: String,
    pub replicas: String,
    pub target_names: String,
    pub app_status: String,
    pub runtime_status: String,
    pub runtime_summary: String,
    pub active_version: String,
    pub health_check: HealthCheckConfig,
    pub health_check_detail: String,
    pub last_health_message: String,
    pub last_health_at: String,
    pub updated_at: String,
    pub target_nodes: Vec<ServiceTargetNodeItem>,
}

#[derive(Clone, Debug)]
pub struct ServiceTargetNodeItem {
    pub id: i64,
    pub name: String,
    pub node_key: String,
    pub runtime_status: String,
    pub active_version: String,
    pub service_count: i64,
    pub message: String,
    pub last_task_id: Option<i64>,
    pub last_task_status: Option<String>,
    pub last_task_kind: Option<String>,
    pub last_deploy_at: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct ServiceLogOutput {
    pub command_output: ComposeCommandOutput,
    pub node: ServiceTargetNodeItem,
    pub target_nodes: Vec<ServiceTargetNodeItem>,
}

#[derive(Clone, Debug)]
pub struct CreateAppInput {
    pub app_key: String,
    pub name: String,
    pub description: String,
    pub environment: String,
    pub app_type: String,
    pub deploy_strategy: String,
    pub release_source: String,
    pub auto_queue_release: bool,
    pub work_dir: String,
    pub target_node_ids: Vec<i64>,
    pub compose_content: String,
    pub env_content: String,
    pub deploy_scripts: DeployScriptSet,
    pub health_check: HealthCheckConfig,
    pub binary_artifact_version: String,
    pub binary_artifact_path: String,
    pub binary_exec_args: String,
    pub binary_service_user: String,
    pub binary_unit_name: String,
    pub binary_release_strategy: String,
    pub binary_active_slot: String,
    pub binary_base_port: i64,
    pub binary_standby_port: i64,
    pub binary_proxy_enabled: bool,
    pub binary_proxy_kind: String,
    pub binary_proxy_domain: String,
    pub binary_proxy_config_path: String,
}

#[derive(Clone, Debug)]
pub struct UpdateAppConfigInput {
    pub app_id: i64,
    pub compose_content: String,
    pub env_content: String,
    pub deploy_scripts: DeployScriptSet,
    pub binary_artifact_version: String,
    pub binary_artifact_path: String,
    pub binary_exec_args: String,
    pub binary_service_user: String,
    pub binary_unit_name: String,
    pub binary_release_strategy: String,
    pub binary_active_slot: String,
    pub binary_base_port: i64,
    pub binary_standby_port: i64,
    pub binary_proxy_enabled: bool,
    pub binary_proxy_kind: String,
    pub binary_proxy_domain: String,
    pub binary_proxy_config_path: String,
    pub health_check: HealthCheckConfig,
}

#[derive(Clone, Debug)]
pub struct UpdateAppMetadataInput {
    pub app_id: i64,
    pub name: String,
    pub description: String,
    pub environment: String,
    pub work_dir: String,
    pub deploy_strategy: String,
    pub release_source: String,
    pub auto_queue_release: bool,
    pub target_node_ids: Vec<i64>,
}

#[derive(Clone, Debug)]
pub struct UploadBinaryArtifactInput {
    pub app_id: i64,
    pub artifact_version: String,
    pub version_code: Option<i64>,
    pub published_at: String,
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub entry_file: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct UploadReleasePackageInput {
    pub app_id: i64,
    pub release_version: String,
    pub version_code: Option<i64>,
    pub published_at: String,
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub entry_file: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct CreateReleasePackageUploadInput {
    pub app_id: i64,
    pub release_version: String,
    pub version_code: Option<i64>,
    pub published_at: String,
    pub file_name: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct CreateReleasePackageUploadResult {
    pub upload_id: String,
    pub app_id: i64,
    pub app_key: String,
    pub release_version: String,
    pub version_code: i64,
    pub file_name: String,
    pub object_key: String,
    pub bucket: String,
    pub endpoint: String,
    pub upload_url: String,
    pub upload_method: String,
    pub upload_headers: Vec<(String, String)>,
    pub expires_at: String,
    pub complete_path: String,
}

#[derive(Clone, Debug)]
pub struct CompleteReleasePackageUploadInput {
    pub upload_id: String,
    pub service_key: String,
    pub checksum_sha256: String,
    pub size_bytes: i64,
    pub published_at: String,
    pub source: String,
}

struct RegisterBinaryArtifactInput {
    app: AppDetailItem,
    artifact_version: String,
    version_code: Option<i64>,
    published_at: String,
    file_name: String,
    bytes: Vec<u8>,
    entry_file: String,
    source: String,
}

#[derive(Clone, Debug)]
pub struct UploadBinaryArtifactResult {
    pub app_id: i64,
    pub app_key: String,
    pub release_id: i64,
    pub queue_id: Option<i64>,
    pub artifact_version: String,
    pub version_code: i64,
    pub artifact_path: String,
    pub artifact_kind: String,
    pub published_at: String,
    pub config_snapshot_id: i64,
    pub config_revision_no: i64,
    pub queued: bool,
    pub publish_status: String,
    pub scheduled_publish_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UploadReleasePackageResult {
    pub app_id: i64,
    pub app_key: String,
    pub release_id: i64,
    pub queue_id: Option<i64>,
    pub release_version: String,
    pub version_code: i64,
    pub package_path: String,
    pub package_kind: String,
    pub published_at: String,
    pub config_snapshot_id: i64,
    pub config_revision_no: i64,
    pub queued: bool,
    pub publish_status: String,
    pub scheduled_publish_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ParsedReleasePackageName {
    pub service_key: String,
    pub release_version: String,
    pub version_code: i64,
}

pub type ParsedBinaryPackageName = ParsedReleasePackageName;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinaryPackageNameError {
    InvalidPackageVersionName,
    ServiceKeyMismatch { expected: String, actual: String },
    PackageVersionConflict { expected: String, actual: String },
}

impl BinaryPackageNameError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPackageVersionName => "INVALID_PACKAGE_VERSION_NAME",
            Self::ServiceKeyMismatch { .. } => "PACKAGE_SERVICE_KEY_MISMATCH",
            Self::PackageVersionConflict { .. } => "PACKAGE_VERSION_CONFLICT",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidPackageVersionName => format!(
                "版本包文件名不符合规范，请使用 {}，例如 {}",
                BINARY_PACKAGE_PATTERN, BINARY_PACKAGE_EXAMPLE
            ),
            Self::ServiceKeyMismatch { expected, actual } => {
                format!("版本包服务标识不匹配，接口路径为 {expected}，文件名中为 {actual}")
            }
            Self::PackageVersionConflict { expected, actual } => {
                format!("版本包发布版本冲突，接口字段为 {expected}，文件名中为 {actual}")
            }
        }
    }
}

struct NormalizeBinaryConfigInput<'a> {
    app_key: &'a str,
    work_dir: &'a str,
    artifact_version: &'a str,
    artifact_path: &'a str,
    exec_args: &'a str,
    service_user: &'a str,
    unit_name: &'a str,
    release_strategy: &'a str,
    active_slot: &'a str,
    base_port: i64,
    standby_port: i64,
    proxy_enabled: bool,
    proxy_kind: &'a str,
    proxy_domain: &'a str,
    proxy_config_path: &'a str,
    env_content: &'a str,
}

impl AppService {
    pub fn new(
        db: SqlitePool,
        runtime_fs: RuntimeFs,
        compose: ComposeExecutor,
        systemd: SystemdExecutor,
        tasks: TaskService,
        platform: PlatformConfigService,
    ) -> Self {
        let compose_queue = ComposeTaskQueue::start(
            db.clone(),
            runtime_fs.clone(),
            compose.clone(),
            systemd.clone(),
            tasks.clone(),
            platform.clone(),
        );
        let binary_queue = BinaryTaskQueue::start(
            db.clone(),
            runtime_fs.clone(),
            compose.clone(),
            systemd.clone(),
            tasks.clone(),
        );
        let release_dispatch_queue = ReleaseDispatchQueue::start(
            db.clone(),
            runtime_fs.clone(),
            tasks.clone(),
            compose_queue.clone(),
        );
        Self {
            db,
            runtime_fs,
            compose,
            systemd,
            tasks,
            compose_queue,
            binary_queue,
            release_dispatch_queue,
            platform,
        }
    }

    pub async fn list_apps(&self) -> Result<Vec<AppListItem>, AppError> {
        sqlx::query_as::<_, AppListItem>(
            r#"
            WITH runtime_counts AS (
                SELECT
                    app_id,
                    COUNT(*) AS node_count,
                    SUM(CASE WHEN runtime_status = 'healthy' THEN 1 ELSE 0 END) AS healthy_count,
                    SUM(CASE WHEN runtime_status = 'unhealthy' THEN 1 ELSE 0 END) AS unhealthy_count,
                    SUM(CASE WHEN runtime_status = 'deploying' THEN 1 ELSE 0 END) AS deploying_count,
                    SUM(CASE WHEN runtime_status = 'stopped' THEN 1 ELSE 0 END) AS stopped_count
                FROM app_runtime_states
                GROUP BY app_id
            )
            SELECT
                a.id,
                a.app_key,
                a.name,
                a.description,
                a.environment,
                a.app_type,
                a.deploy_mode,
                a.deploy_strategy,
                a.release_source,
                a.compose_strategy,
                a.auto_queue_release,
                a.work_dir,
                a.status,
                CASE
                    WHEN a.status = 'disabled' THEN 'disabled'
                    WHEN COALESCE(rc.deploying_count, 0) > 0 THEN 'deploying'
                    WHEN COALESCE(rc.unhealthy_count, 0) > 0 THEN 'unhealthy'
                    WHEN COALESCE(rc.healthy_count, 0) > 0
                        AND COALESCE(rc.healthy_count, 0) = COALESCE(rc.node_count, 0)
                        THEN 'healthy'
                    WHEN COALESCE(rc.stopped_count, 0) > 0
                        AND COALESCE(rc.stopped_count, 0) = COALESCE(rc.node_count, 0)
                        THEN 'stopped'
                    ELSE 'unknown'
                END AS runtime_status,
                CASE
                    WHEN a.status = 'disabled' THEN '应用已停用'
                    WHEN COALESCE(rc.node_count, 0) = 0 THEN '暂无节点运行记录'
                    ELSE
                        COALESCE(rc.healthy_count, 0) || ' 健康，'
                        || COALESCE(rc.unhealthy_count, 0) || ' 异常，'
                        || COALESCE(rc.deploying_count, 0) || ' 部署中，'
                        || COALESCE(rc.stopped_count, 0) || ' 已停止，'
                        || (
                            COALESCE(rc.node_count, 0)
                            - COALESCE(rc.healthy_count, 0)
                            - COALESCE(rc.unhealthy_count, 0)
                            - COALESCE(rc.deploying_count, 0)
                            - COALESCE(rc.stopped_count, 0)
                        ) || ' 未知'
                END AS runtime_summary,
                group_concat(n.name, '、') AS target_names,
                COUNT(n.id) AS target_count,
                a.created_at,
                a.updated_at
            FROM apps a
            LEFT JOIN app_targets t ON t.app_id = a.id
            LEFT JOIN nodes n ON n.id = t.node_id
            LEFT JOIN runtime_counts rc ON rc.app_id = a.id
            GROUP BY a.id
            ORDER BY a.id DESC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    pub async fn node_options(&self) -> Result<Vec<AppNodeOption>, AppError> {
        sqlx::query_as::<_, AppNodeOption>(
            r#"
            SELECT id, name, node_key
            FROM nodes
            WHERE status != 'disabled'
            ORDER BY CASE node_key WHEN 'local' THEN 0 ELSE 1 END, id DESC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    pub async fn app_detail(&self, app_id: i64) -> Result<AppConfigDetail, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let service_names = if app.app_type == "compose" {
            parse_compose_services(&runtime_files.compose_content)?
                .into_iter()
                .map(|service| service.name)
                .collect::<Vec<_>>()
        } else {
            vec![app.app_key.clone()]
        };
        let health_check = load_health_check_config(&self.db, app.id).await?;
        let deployment_runs = self.list_app_deployment_runs(app.id).await?;
        let config_snapshots = self.list_app_config_snapshots(app.id).await?;
        let deploy_snapshot = self.latest_deploy_snapshot(app.id).await?;
        let runtime_states = self.list_app_runtime_states(app.id).await?;
        let target_nodes = self.list_app_target_summaries(app.id).await?;
        let target_choices = self.list_app_target_choices(app.id).await?;
        let binary_config = self.load_binary_config(&app).await?;
        let binary_releases = if app.app_type == "binary" {
            self.list_binary_artifacts(app.id).await?
        } else {
            Vec::new()
        };
        let binary_runtime = if app.app_type == "binary" {
            self.runtime_fs
                .load_binary_runtime_files(
                    &app.app_key,
                    &binary_config.unit_name,
                    &binary_config.artifact_version,
                )
                .await?
        } else {
            BinaryRuntimeFiles::default()
        };
        let deploy_diff = build_deploy_diff(
            &app,
            &runtime_files.compose_content,
            &runtime_files.env_content,
            &binary_config,
            deploy_snapshot.as_ref(),
        );

        Ok(AppConfigDetail {
            app,
            runtime_root: runtime_files.root_dir.to_string_lossy().to_string(),
            compose_content: runtime_files.compose_content,
            env_content: runtime_files.env_content,
            deploy_scripts: runtime_files.deploy_scripts,
            metadata_content: runtime_files.metadata_content,
            service_names,
            binary_runtime,
            health_check,
            deployment_runs,
            config_snapshots,
            deploy_diff,
            runtime_states,
            target_nodes,
            target_choices,
            binary_config,
            binary_releases,
        })
    }

    pub async fn app_id_by_key(&self, app_key: &str) -> Result<i64, AppError> {
        let app_key = normalize_key(app_key)?;
        sqlx::query_scalar::<_, i64>("SELECT id FROM apps WHERE app_key = ?1")
            .bind(app_key)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| AppError::InvalidInput("服务标识不存在".to_owned()))
    }

    pub async fn node_ids_by_keys(&self, node_keys: &[String]) -> Result<Vec<i64>, AppError> {
        let mut node_ids = Vec::new();
        for node_key in dedupe_strings(node_keys) {
            let node_key = normalize_key(&node_key)?;
            let node_id = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM nodes WHERE node_key = ?1 AND status != 'disabled'",
            )
            .bind(&node_key)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| AppError::InvalidInput(format!("目标节点 {node_key} 不存在或已禁用")))?;
            node_ids.push(node_id);
        }
        Ok(node_ids)
    }

    async fn list_app_target_summaries(
        &self,
        app_id: i64,
    ) -> Result<Vec<AppTargetSummaryItem>, AppError> {
        sqlx::query_as::<_, AppTargetSummaryItem>(
            r#"
            SELECT
                n.id,
                n.name,
                n.node_key,
                n.node_type,
                n.status,
                n.docker_status,
                COALESCE(c.check_status, 'unknown') AS capability_status,
                COALESCE(c.docker_available, 0) AS docker_available,
                COALESCE(c.compose_available, 0) AS compose_available,
                COALESCE(c.systemd_available, 0) AS systemd_available,
                COALESCE(c.caddy_available, 0) AS caddy_available,
                COALESCE(c.nginx_available, 0) AS nginx_available,
                COALESCE(c.message, '') AS capability_message
            FROM nodes n
            JOIN app_targets t ON t.node_id = n.id
            LEFT JOIN node_capabilities c ON c.node_id = n.id
            WHERE t.app_id = ?1
              AND n.status != 'disabled'
            ORDER BY n.id
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn list_app_deployment_runs(
        &self,
        app_id: i64,
    ) -> Result<Vec<AppDeploymentRunItem>, AppError> {
        sqlx::query_as::<_, AppDeploymentRunItem>(
            r#"
            SELECT
                r.id,
                r.task_id,
                t.title AS task_title,
                r.deploy_action,
                r.status,
                r.message,
                r.config_snapshot_id,
                r.config_revision_no,
                r.artifact_version,
                r.started_at,
                r.finished_at
            FROM deployment_runs r
            LEFT JOIN operation_tasks t ON t.id = r.task_id
            WHERE r.app_id = ?1
            ORDER BY r.id DESC
            LIMIT 10
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn list_app_config_snapshots(
        &self,
        app_id: i64,
    ) -> Result<Vec<AppConfigSnapshotItem>, AppError> {
        sqlx::query_as::<_, AppConfigSnapshotItem>(
            r#"
            SELECT
                id,
                revision_no,
                snapshot_kind,
                compose_content,
                env_content,
                artifact_version,
                config_hash,
                metadata,
                created_at
            FROM app_config_snapshots
            WHERE app_id = ?1
            ORDER BY id DESC
            LIMIT 10
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn latest_deploy_snapshot(
        &self,
        app_id: i64,
    ) -> Result<Option<AppConfigSnapshotItem>, AppError> {
        sqlx::query_as::<_, AppConfigSnapshotItem>(
            r#"
            SELECT
                id,
                revision_no,
                snapshot_kind,
                compose_content,
                env_content,
                artifact_version,
                config_hash,
                metadata,
                created_at
            FROM app_config_snapshots
            WHERE app_id = ?1
              AND snapshot_kind = 'deploy'
            ORDER BY id DESC
            LIMIT 1
            "#,
        )
        .bind(app_id)
        .fetch_optional(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn latest_config_snapshot(
        &self,
        app_id: i64,
    ) -> Result<Option<RuntimeConfigSnapshotRecord>, AppError> {
        sqlx::query_as::<_, RuntimeConfigSnapshotRecord>(
            r#"
            SELECT id, revision_no
            FROM app_config_snapshots
            WHERE app_id = ?1
            ORDER BY id DESC
            LIMIT 1
            "#,
        )
        .bind(app_id)
        .fetch_optional(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn list_app_runtime_states(
        &self,
        app_id: i64,
    ) -> Result<Vec<AppRuntimeStateItem>, AppError> {
        sqlx::query_as::<_, AppRuntimeStateItem>(
            r#"
            SELECT
                n.id AS node_id,
                n.name AS node_name,
                n.node_key,
                s.runtime_status,
                s.active_version,
                s.service_count,
                s.message,
                s.last_task_id,
                t.status AS last_task_status,
                t.task_kind AS last_task_kind,
                s.last_deploy_at,
                s.updated_at
            FROM app_runtime_states s
            JOIN nodes n ON n.id = s.node_id
            LEFT JOIN operation_tasks t ON t.id = s.last_task_id
            WHERE s.app_id = ?1
            ORDER BY n.id
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn list_app_target_choices(
        &self,
        app_id: i64,
    ) -> Result<Vec<AppTargetChoiceItem>, AppError> {
        sqlx::query_as::<_, AppTargetChoiceItem>(
            r#"
            SELECT
                n.id,
                n.name,
                n.node_key,
                EXISTS(
                    SELECT 1
                    FROM app_targets t
                    WHERE t.app_id = ?1
                      AND t.node_id = n.id
                ) AS checked
            FROM nodes n
            WHERE n.status != 'disabled'
               OR EXISTS(
                    SELECT 1
                    FROM app_targets t
                    WHERE t.app_id = ?1
                      AND t.node_id = n.id
               )
            ORDER BY CASE n.node_key WHEN 'local' THEN 0 ELSE 1 END, n.id DESC
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn load_binary_config(&self, app: &AppDetailItem) -> Result<BinaryConfigItem, AppError> {
        if app.app_type != "binary" {
            return Ok(BinaryConfigItem::default());
        }
        self.load_binary_config_for_app(app.id, &app.app_key, &app.work_dir)
            .await
    }

    async fn load_binary_config_for_app(
        &self,
        app_id: i64,
        app_key: &str,
        work_dir: &str,
    ) -> Result<BinaryConfigItem, AppError> {
        let config = sqlx::query_as::<_, BinaryConfigItem>(
            r#"
            SELECT
                service_name,
                artifact_version,
                artifact_path,
                exec_args,
                working_dir,
                service_user,
                unit_name,
                release_strategy,
                active_slot,
                base_port,
                standby_port,
                proxy_enabled,
                proxy_kind,
                proxy_domain,
                proxy_config_path,
                env_content
            FROM app_binary_configs
            WHERE app_id = ?1
            "#,
        )
        .bind(app_id)
        .fetch_optional(&self.db)
        .await?;
        Ok(config.unwrap_or_else(|| default_binary_config_for_app(app_key, work_dir)))
    }

    async fn list_binary_artifacts(
        &self,
        app_id: i64,
    ) -> Result<Vec<BinaryArtifactItem>, AppError> {
        sqlx::query_as::<_, BinaryArtifactItem>(
            r#"
            SELECT
                id,
                version,
                version_code,
                artifact_path,
                artifact_kind,
                status,
                metadata,
                published_at,
                created_at
            FROM binary_artifacts
            WHERE app_id = ?1
            ORDER BY version_code DESC, published_at DESC, id DESC
            LIMIT 20
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn binary_artifact_by_version(
        &self,
        app_id: i64,
        version: &str,
    ) -> Result<Option<BinaryArtifactItem>, AppError> {
        sqlx::query_as::<_, BinaryArtifactItem>(
            r#"
            SELECT
                id,
                version,
                version_code,
                artifact_path,
                artifact_kind,
                status,
                metadata,
                published_at,
                created_at
            FROM binary_artifacts
            WHERE app_id = ?1
              AND version = ?2
            "#,
        )
        .bind(app_id)
        .bind(version)
        .fetch_optional(&self.db)
        .await
        .map_err(AppError::from)
    }

    pub async fn list_app_releases(&self) -> Result<Vec<AppReleaseItem>, AppError> {
        sqlx::query_as::<_, AppReleaseItem>(
            r#"
            SELECT
                r.id,
                r.app_id,
                a.name AS app_name,
                a.app_key,
                r.version,
                r.version_code,
                r.package_name,
                r.package_path,
                r.extract_dir,
                r.status,
                r.source,
                r.checksum_sha256,
                r.size_bytes,
                r.published_at,
                r.received_at,
                (
                    SELECT q.scheduled_publish_at
                    FROM app_release_queue q
                    WHERE q.release_id = r.id
                      AND q.status IN ('scheduled', 'queued', 'running')
                    ORDER BY q.id DESC
                    LIMIT 1
                ) AS scheduled_publish_at,
                r.storage_provider,
                r.storage_bucket,
                r.storage_object_key,
                r.storage_endpoint,
                r.metadata
            FROM app_releases r
            JOIN apps a ON a.id = r.app_id
            ORDER BY r.version_code DESC, r.published_at DESC, r.id DESC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    pub async fn list_app_release_queue(&self) -> Result<Vec<AppReleaseQueueItem>, AppError> {
        sqlx::query_as::<_, AppReleaseQueueItem>(
            r#"
            SELECT
                q.id,
                q.app_id,
                a.name AS app_name,
                a.app_key,
                q.release_id,
                r.version,
                r.version_code,
                q.config_snapshot_id,
                q.queue_seq,
                q.status,
                q.triggered_by,
                q.message,
                q.task_id,
                q.scheduled_publish_at,
                q.created_at,
                q.started_at,
                q.finished_at
            FROM app_release_queue q
            JOIN apps a ON a.id = q.app_id
            JOIN app_releases r ON r.id = q.release_id
            ORDER BY q.queue_seq ASC, q.id ASC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    pub async fn publish_release_now(
        &self,
        release_id: i64,
        actor: &str,
    ) -> Result<Option<i64>, AppError> {
        let release = sqlx::query_as::<_, AppReleaseItem>(
            r#"
            SELECT
                r.id,
                r.app_id,
                a.name AS app_name,
                a.app_key,
                r.version,
                r.version_code,
                r.package_name,
                r.package_path,
                r.extract_dir,
                r.status,
                r.source,
                r.checksum_sha256,
                r.size_bytes,
                r.published_at,
                r.received_at,
                (
                    SELECT q.scheduled_publish_at
                    FROM app_release_queue q
                    WHERE q.release_id = r.id
                      AND q.status IN ('scheduled', 'queued', 'running')
                    ORDER BY q.id DESC
                    LIMIT 1
                ) AS scheduled_publish_at,
                r.storage_provider,
                r.storage_bucket,
                r.storage_object_key,
                r.storage_endpoint,
                r.metadata
            FROM app_releases r
            JOIN apps a ON a.id = r.app_id
            WHERE r.id = ?1
            "#,
        )
        .bind(release_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("发布版本不存在".to_owned()))?;

        let app = self.fetch_app_detail(release.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_compose_app(&app)?;
        if matches!(release.status.as_str(), "queued" | "deploying") {
            return Err(AppError::Conflict("该发布版本已经在发布队列中".to_owned()));
        }

        let snapshot_id = self
            .resolve_release_snapshot_id(release.app_id, &release.metadata)
            .await?;
        let mut tx = self.db.begin().await?;
        let queue_id = enqueue_app_release(
            &mut tx,
            release.app_id,
            release.id,
            snapshot_id,
            actor,
            "手动加入发布队列",
            "queued",
            None,
        )
        .await
        .map(Some)
        .or_else(|err| match err {
            sqlx::Error::Database(db_err) if db_err.is_unique_violation() => Ok(None),
            other => Err(other),
        })?;
        sqlx::query(
            r#"
            UPDATE app_releases
            SET status = 'queued',
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(release.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.enqueue_release_dispatch(release.app_id).await?;
        Ok(queue_id)
    }

    pub async fn schedule_release_publish(
        &self,
        release_id: i64,
        scheduled_publish_at: &str,
    ) -> Result<String, AppError> {
        let scheduled_publish_at = normalize_published_at(scheduled_publish_at)?
            .ok_or_else(|| AppError::InvalidInput("请填写计划发布时间".to_owned()))?;
        let release = sqlx::query_as::<_, AppReleaseItem>(
            r#"
            SELECT
                r.id,
                r.app_id,
                a.name AS app_name,
                a.app_key,
                r.version,
                r.version_code,
                r.package_name,
                r.package_path,
                r.extract_dir,
                r.status,
                r.source,
                r.checksum_sha256,
                r.size_bytes,
                r.published_at,
                r.received_at,
                (
                    SELECT q.scheduled_publish_at
                    FROM app_release_queue q
                    WHERE q.release_id = r.id
                      AND q.status IN ('scheduled', 'queued', 'running')
                    ORDER BY q.id DESC
                    LIMIT 1
                ) AS scheduled_publish_at,
                r.storage_provider,
                r.storage_bucket,
                r.storage_object_key,
                r.storage_endpoint,
                r.metadata
            FROM app_releases r
            JOIN apps a ON a.id = r.app_id
            WHERE r.id = ?1
            "#,
        )
        .bind(release_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("发布版本不存在".to_owned()))?;

        let app = self.fetch_app_detail(release.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_compose_app(&app)?;
        if matches!(release.status.as_str(), "queued" | "deploying") {
            return Err(AppError::Conflict("该发布版本已经在发布队列中".to_owned()));
        }

        let snapshot_id = self
            .resolve_release_snapshot_id(release.app_id, &release.metadata)
            .await?;
        let metadata = release_metadata_with_snapshot(
            &release.metadata,
            snapshot_id,
            Some(release.version_code),
        )?;
        let mut tx = self.db.begin().await?;
        enqueue_app_release(
            &mut tx,
            release.app_id,
            release.id,
            snapshot_id,
            "scheduler",
            &format!("计划在 {scheduled_publish_at} 发布"),
            "scheduled",
            Some(&scheduled_publish_at),
        )
        .await
        .map_err(AppError::from)?;
        sqlx::query(
            r#"
            UPDATE app_releases
            SET status = 'queued',
                metadata = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(release.id)
        .bind(&metadata)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(scheduled_publish_at)
    }

    pub async fn cancel_scheduled_release(&self, release_id: i64) -> Result<(), AppError> {
        let mut tx = self.db.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE app_release_queue
            SET status = 'canceled',
                message = '已取消定时发布',
                scheduled_publish_at = NULL,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE release_id = ?1
              AND status = 'scheduled'
              AND scheduled_publish_at IS NOT NULL
              AND scheduled_publish_at != ''
            "#,
        )
        .bind(release_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::InvalidInput(
                "当前发布版本没有待执行的定时发布".to_owned(),
            ));
        }
        sqlx::query(
            r#"
            UPDATE app_releases
            SET status = 'received',
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(release_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn cancel_release_queue_item(&self, queue_id: i64) -> Result<i64, AppError> {
        let queue = sqlx::query_as::<_, (i64, i64, String)>(
            r#"
            SELECT app_id, release_id, status
            FROM app_release_queue
            WHERE id = ?1
            "#,
        )
        .bind(queue_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("发布队列项不存在".to_owned()))?;
        if !matches!(queue.2.as_str(), "queued" | "scheduled") {
            return Err(AppError::InvalidInput(
                "只能取消等待中或定时等待的发布队列项".to_owned(),
            ));
        }

        let mut tx = self.db.begin().await?;
        sqlx::query(
            r#"
            UPDATE app_release_queue
            SET status = 'canceled',
                message = '已取消待发布队列',
                scheduled_publish_at = NULL,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(queue_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE app_releases
            SET status = 'received',
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(queue.1)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.enqueue_release_dispatch(queue.0).await?;
        Ok(queue.0)
    }

    async fn resolve_release_snapshot_id(
        &self,
        app_id: i64,
        metadata: &str,
    ) -> Result<i64, AppError> {
        if let Some(snapshot_id) = release_metadata_snapshot_id(metadata) {
            return Ok(snapshot_id);
        }
        let snapshot = self
            .latest_config_snapshot(app_id)
            .await?
            .ok_or_else(|| AppError::InvalidInput("当前应用还没有可用配置快照".to_owned()))?;
        Ok(snapshot.id)
    }

    async fn enqueue_release_dispatch(&self, app_id: i64) -> Result<(), AppError> {
        self.release_dispatch_queue.enqueue(app_id).await
    }

    pub async fn list_services(&self) -> Result<Vec<ServiceListItem>, AppError> {
        let apps = self.list_apps().await?;
        let mut services = Vec::new();
        for app in apps {
            let health_check = load_health_check_config(&self.db, app.id).await?;
            let runtime_states = self.list_app_runtime_states(app.id).await?;
            let target_nodes = service_target_node_items(
                &self.target_node_metadata_for_app(app.id).await?,
                &runtime_states,
            );
            let runtime_overview = service_runtime_overview(&runtime_states);
            let target_names = app
                .target_names
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("未绑定节点")
                .to_owned();
            if app.app_type == "compose" {
                let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
                let mut compose_services = parse_compose_services(&runtime_files.compose_content)?;
                if compose_services.is_empty() {
                    compose_services.push(ParsedService {
                        name: "未解析服务".to_owned(),
                        image: "未配置镜像".to_owned(),
                        ports: "未声明端口".to_owned(),
                        replicas: "1".to_owned(),
                    });
                }
                for service in compose_services {
                    services.push(ServiceListItem {
                        app_id: app.id,
                        app_name: app.name.clone(),
                        app_key: app.app_key.clone(),
                        service_name: service.name,
                        service_kind: "Docker Compose".to_owned(),
                        image: service.image,
                        ports: service.ports,
                        replicas: service.replicas,
                        target_names: target_names.clone(),
                        app_status: app.status.clone(),
                        runtime_status: runtime_overview.status.clone(),
                        runtime_summary: runtime_overview.summary.clone(),
                        active_version: runtime_overview.active_version.clone(),
                        health_check: health_check.clone(),
                        health_check_detail: health_check_detail_text(&health_check, None),
                        last_health_message: runtime_overview.latest_message.clone(),
                        last_health_at: runtime_overview.latest_checked_at.clone(),
                        updated_at: app.updated_at.clone(),
                        target_nodes: target_nodes.clone(),
                    });
                }
            } else {
                let binary_config = self
                    .load_binary_config_for_app(app.id, &app.app_key, &app.work_dir)
                    .await?;
                let health_check_detail =
                    health_check_detail_text(&health_check, Some(&binary_config));
                services.push(ServiceListItem {
                    app_id: app.id,
                    app_name: app.name.clone(),
                    app_key: app.app_key.clone(),
                    service_name: binary_config.service_name.clone(),
                    service_kind: "二进制".to_owned(),
                    image: "systemd 服务".to_owned(),
                    ports: "待配置".to_owned(),
                    replicas: "1".to_owned(),
                    target_names,
                    app_status: app.status,
                    runtime_status: runtime_overview.status,
                    runtime_summary: runtime_overview.summary,
                    active_version: runtime_overview.active_version,
                    health_check,
                    health_check_detail,
                    last_health_message: runtime_overview.latest_message,
                    last_health_at: runtime_overview.latest_checked_at,
                    updated_at: app.updated_at,
                    target_nodes,
                });
            }
        }
        Ok(services)
    }

    pub async fn create_app(&self, input: CreateAppInput) -> Result<i64, AppError> {
        let app_key = normalize_key(&input.app_key)?;
        let name = required_text(&input.name, "请输入应用名称")?;
        let app_type = normalize_app_type(&input.app_type)?;
        let environment = normalize_app_environment(&input.environment)?;
        let deploy_mode = app_type.clone();
        let deploy_strategy = normalize_deploy_strategy(&input.deploy_strategy)?;
        let release_source = normalize_release_source(&input.release_source)?;
        let auto_queue_release = input.auto_queue_release;
        let work_dir = normalize_deploy_work_dir(&input.work_dir)?;
        let description = input.description.trim().to_owned();
        let runtime_name = name.clone();
        let compose_content = if app_type == "compose" {
            normalize_compose_content(&input.compose_content, &app_key)?
        } else {
            String::new()
        };
        let env_content = normalize_env_content(&input.env_content);
        let binary_config = if app_type == "binary" {
            Some(normalize_binary_config(NormalizeBinaryConfigInput {
                app_key: &app_key,
                work_dir: &work_dir,
                artifact_version: &input.binary_artifact_version,
                artifact_path: &input.binary_artifact_path,
                exec_args: &input.binary_exec_args,
                service_user: &input.binary_service_user,
                unit_name: &input.binary_unit_name,
                release_strategy: &input.binary_release_strategy,
                active_slot: &input.binary_active_slot,
                base_port: input.binary_base_port,
                standby_port: input.binary_standby_port,
                proxy_enabled: input.binary_proxy_enabled,
                proxy_kind: &input.binary_proxy_kind,
                proxy_domain: &input.binary_proxy_domain,
                proxy_config_path: &input.binary_proxy_config_path,
                env_content: &input.env_content,
            })?)
        } else {
            None
        };
        if input.target_node_ids.is_empty() {
            return Err(AppError::InvalidInput(
                "至少需要选择一个目标节点".to_owned(),
            ));
        }

        let target_nodes = self.target_node_metadata(&input.target_node_ids).await?;
        let missing_target = input
            .target_node_ids
            .iter()
            .any(|node_id| !target_nodes.iter().any(|node| node.id == *node_id));
        if missing_target {
            return Err(AppError::InvalidInput("目标节点不存在".to_owned()));
        }
        let mut tx = self.db.begin().await?;
        let app_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO apps(
                app_key,
                name,
                description,
                environment,
                app_type,
                deploy_mode,
                deploy_strategy,
                release_source,
                auto_queue_release,
                work_dir,
                status
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'ready')
            RETURNING id
            "#,
        )
        .bind(&app_key)
        .bind(&name)
        .bind(&description)
        .bind(&environment)
        .bind(&app_type)
        .bind(&deploy_mode)
        .bind(&deploy_strategy)
        .bind(&release_source)
        .bind(if auto_queue_release { 1 } else { 0 })
        .bind(&work_dir)
        .fetch_one(&mut *tx)
        .await?;

        for node_id in dedupe_ids(&input.target_node_ids) {
            sqlx::query(
                "INSERT INTO app_targets(app_id, node_id, target_role) VALUES (?1, ?2, 'primary')",
            )
            .bind(app_id)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO app_runtime_states(app_id, node_id, runtime_status, message)
                VALUES (?1, ?2, 'unknown', '等待首次部署')
                "#,
            )
            .bind(app_id)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            r#"
            INSERT INTO app_health_checks(app_id, check_kind)
            VALUES (?1, ?2)
            "#,
        )
        .bind(app_id)
        .bind(input.health_check.kind.as_str())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE app_health_checks
            SET endpoint = ?2,
                timeout_secs = ?3,
                expected_status = ?4,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE app_id = ?1
            "#,
        )
        .bind(app_id)
        .bind(&input.health_check.endpoint)
        .bind(input.health_check.timeout_secs as i64)
        .bind(input.health_check.expected_status as i64)
        .execute(&mut *tx)
        .await?;

        if let Some(config) = &binary_config {
            sqlx::query(
                r#"
                INSERT INTO app_binary_configs(
                    app_id,
                    service_name,
                    artifact_version,
                    artifact_path,
                    exec_args,
                    working_dir,
                    service_user,
                    unit_name,
                    release_strategy,
                    active_slot,
                    base_port,
                    standby_port,
                    proxy_enabled,
                    proxy_kind,
                    proxy_domain,
                    proxy_config_path,
                    env_content
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                "#,
            )
            .bind(app_id)
            .bind(&config.service_name)
            .bind(&config.artifact_version)
            .bind(&config.artifact_path)
            .bind(&config.exec_args)
            .bind(&config.working_dir)
            .bind(&config.service_user)
            .bind(&config.unit_name)
            .bind(&config.release_strategy)
            .bind(&config.active_slot)
            .bind(config.base_port)
            .bind(config.standby_port)
            .bind(config.proxy_enabled)
            .bind(&config.proxy_kind)
            .bind(&config.proxy_domain)
            .bind(&config.proxy_config_path)
            .bind(&config.env_content)
            .execute(&mut *tx)
            .await?;
            if !config.artifact_version.trim().is_empty() {
                sqlx::query(
                    r#"
            INSERT INTO binary_artifacts(
                app_id,
                version,
                version_code,
                artifact_path,
                artifact_kind,
                status,
                published_at,
                metadata
            )
            VALUES (?1, ?2, ?3, ?4, 'binary', 'registered', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?5)
            ON CONFLICT(app_id, version) DO UPDATE SET
                version_code = excluded.version_code,
                artifact_path = excluded.artifact_path,
                status = 'registered',
                published_at = excluded.published_at,
                metadata = excluded.metadata
        "#,
    )
    .bind(app_id)
    .bind(&config.artifact_version)
    .bind(version_code_from_release(&config.artifact_version).unwrap_or_default())
    .bind(&config.artifact_path)
    .bind(format!(
        "{{\"source\":\"manual\",\"unit_name\":\"{}\"}}",
                    json_escape(&config.unit_name)
                ))
                .execute(&mut *tx)
                .await?;
            }
        }

        let initial_artifact_version = binary_config
            .as_ref()
            .map(|config| config.artifact_version.as_str())
            .unwrap_or("");
        let runtime_result = self
            .runtime_fs
            .save_app_config(AppRuntimeConfig {
                app_key: app_key.clone(),
                app_id,
                name: name.clone(),
                description: description.clone(),
                environment: environment.clone(),
                app_type: app_type.clone(),
                deploy_mode: deploy_mode.clone(),
                deploy_strategy: deploy_strategy.clone(),
                deploy_work_dir: work_dir.clone(),
                target_nodes: target_nodes
                    .into_iter()
                    .map(|node| TargetNodeMetadata {
                        node_key: node.node_key,
                        name: node.name,
                    })
                    .collect(),
                compose_content: compose_content.clone(),
                env_content: env_content.clone(),
                deploy_scripts: input.deploy_scripts.clone(),
                binary: binary_config.as_ref().map(to_binary_runtime_metadata),
            })
            .await?;
        if let Some(config) = binary_config
            .as_ref()
            .filter(|config| !config.artifact_version.trim().is_empty())
        {
            self.runtime_fs
                .save_binary_runtime_files(to_binary_runtime_config(
                    app_id,
                    &app_key,
                    &runtime_name,
                    config,
                ))
                .await?;
        }
        let initial_metadata = runtime_snapshot_metadata(
            "manual",
            runtime_result.root_dir.to_string_lossy(),
            None,
            Some(&input.deploy_scripts),
            binary_config.as_ref(),
        );
        let initial_snapshot = insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id,
                snapshot_kind: "initial",
                compose_content: &compose_content,
                env_content: &env_content,
                artifact_version: initial_artifact_version,
                metadata: initial_metadata.clone(),
            },
        )
        .await?;

        sqlx::query(
            r#"
            UPDATE app_config_snapshots
            SET metadata = ?2,
                config_hash = ?3
            WHERE id = ?1
            "#,
        )
        .bind(initial_snapshot.id)
        .bind(&initial_metadata)
        .bind(runtime_config_hash(
            &compose_content,
            &env_content,
            initial_artifact_version,
            &initial_metadata,
        ))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(app_id)
    }

    pub async fn update_app_config(&self, input: UpdateAppConfigInput) -> Result<(), AppError> {
        let app = self.fetch_app_detail(input.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        let compose_content = if app.app_type == "compose" {
            normalize_compose_content(&input.compose_content, &app.app_key)?
        } else {
            String::new()
        };
        let env_content = normalize_env_content(&input.env_content);
        let binary_config = if app.app_type == "binary" {
            Some(normalize_binary_config(NormalizeBinaryConfigInput {
                app_key: &app.app_key,
                work_dir: &app.work_dir,
                artifact_version: &input.binary_artifact_version,
                artifact_path: &input.binary_artifact_path,
                exec_args: &input.binary_exec_args,
                service_user: &input.binary_service_user,
                unit_name: &input.binary_unit_name,
                release_strategy: &input.binary_release_strategy,
                active_slot: &input.binary_active_slot,
                base_port: input.binary_base_port,
                standby_port: input.binary_standby_port,
                proxy_enabled: input.binary_proxy_enabled,
                proxy_kind: &input.binary_proxy_kind,
                proxy_domain: &input.binary_proxy_domain,
                proxy_config_path: &input.binary_proxy_config_path,
                env_content: &input.env_content,
            })?)
        } else {
            None
        };
        let target_nodes = self.target_node_metadata_for_app(app.id).await?;
        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let metadata_content = render_runtime_metadata(
            &app,
            target_nodes
                .iter()
                .map(|node| TargetNodeMetadata {
                    node_key: node.node_key.clone(),
                    name: node.name.clone(),
                })
                .collect(),
            &runtime_root.to_string_lossy(),
            binary_config.as_ref(),
        );
        self.runtime_fs
            .save_app_runtime_files_with_scripts(
                &app.app_key,
                &compose_content,
                &env_content,
                &metadata_content,
                &input.deploy_scripts,
            )
            .await?;
        if let Some(config) = &binary_config {
            self.runtime_fs
                .save_binary_runtime_files(to_binary_runtime_config(
                    app.id,
                    &app.app_key,
                    &app.name,
                    config,
                ))
                .await?;
        }

        let mut tx = self.db.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO app_health_checks(
                app_id,
                check_kind,
                endpoint,
                timeout_secs,
                expected_status,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            ON CONFLICT(app_id) DO UPDATE SET
                check_kind = excluded.check_kind,
                endpoint = excluded.endpoint,
                timeout_secs = excluded.timeout_secs,
                expected_status = excluded.expected_status,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(app.id)
        .bind(input.health_check.kind.as_str())
        .bind(&input.health_check.endpoint)
        .bind(input.health_check.timeout_secs as i64)
        .bind(input.health_check.expected_status as i64)
        .execute(&mut *tx)
        .await?;
        if let Some(config) = &binary_config {
            upsert_binary_config(&mut tx, app.id, config).await?;
        }
        let artifact_version = binary_config
            .as_ref()
            .map(|config| config.artifact_version.as_str())
            .unwrap_or("");
        insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id: app.id,
                snapshot_kind: "manual",
                compose_content: &compose_content,
                env_content: &env_content,
                artifact_version,
                metadata: runtime_snapshot_metadata(
                    "manual",
                    runtime_root.to_string_lossy(),
                    None,
                    Some(&input.deploy_scripts),
                    binary_config.as_ref(),
                ),
            },
        )
        .await?;
        sqlx::query(
            r#"
            UPDATE apps
            SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn update_app_metadata(&self, input: UpdateAppMetadataInput) -> Result<(), AppError> {
        let mut app = self.fetch_app_detail(input.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        let name = required_text(&input.name, "请输入应用名称")?;
        let description = input.description.trim().to_owned();
        let environment = normalize_app_environment(&input.environment)?;
        let work_dir = normalize_deploy_work_dir(&input.work_dir)?;
        let deploy_strategy = normalize_deploy_strategy(&input.deploy_strategy)?;
        let release_source = normalize_release_source(&input.release_source)?;
        let auto_queue_release = input.auto_queue_release;
        if input.target_node_ids.is_empty() {
            return Err(AppError::InvalidInput(
                "至少需要选择一个目标节点".to_owned(),
            ));
        }
        let target_nodes = self.target_node_metadata(&input.target_node_ids).await?;
        let missing_target = input
            .target_node_ids
            .iter()
            .any(|node_id| !target_nodes.iter().any(|node| node.id == *node_id));
        if missing_target {
            return Err(AppError::InvalidInput("目标节点不存在或已禁用".to_owned()));
        }

        let previous_work_dir = app.work_dir.clone();
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let mut binary_config = self.load_binary_config(&app).await?;
        app.name = name.clone();
        app.description = description.clone();
        app.environment = environment.clone();
        app.work_dir = work_dir.clone();
        app.deploy_strategy = deploy_strategy.clone();
        app.release_source = release_source.clone();
        app.auto_queue_release = if auto_queue_release { 1 } else { 0 };
        let binary_config_for_metadata = if app.app_type == "binary" {
            if binary_config.working_dir.trim().is_empty()
                || binary_config.working_dir == previous_work_dir
            {
                binary_config.working_dir = work_dir.clone();
            }
            Some(binary_config.clone())
        } else {
            None
        };
        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let metadata_content = render_runtime_metadata(
            &app,
            target_nodes
                .iter()
                .map(|node| TargetNodeMetadata {
                    node_key: node.node_key.clone(),
                    name: node.name.clone(),
                })
                .collect(),
            &runtime_root.to_string_lossy(),
            binary_config_for_metadata.as_ref(),
        );
        self.runtime_fs
            .save_app_runtime_files(
                &app.app_key,
                &runtime_files.compose_content,
                &runtime_files.env_content,
                &metadata_content,
            )
            .await?;
        if let Some(config) = &binary_config_for_metadata {
            self.runtime_fs
                .save_binary_runtime_files(to_binary_runtime_config(
                    app.id,
                    &app.app_key,
                    &app.name,
                    config,
                ))
                .await?;
        }

        let target_node_ids = dedupe_ids(&input.target_node_ids);
        let mut tx = self.db.begin().await?;
        sqlx::query(
            r#"
            UPDATE apps
            SET name = ?2,
                description = ?3,
                environment = ?4,
                work_dir = ?5,
                deploy_strategy = ?6,
                release_source = ?7,
                auto_queue_release = ?8,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(app.id)
        .bind(&app.name)
        .bind(&app.description)
        .bind(&app.environment)
        .bind(&app.work_dir)
        .bind(&app.deploy_strategy)
        .bind(&app.release_source)
        .bind(app.auto_queue_release)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM app_targets WHERE app_id = ?1")
            .bind(app.id)
            .execute(&mut *tx)
            .await?;
        for node_id in target_node_ids {
            sqlx::query(
                "INSERT INTO app_targets(app_id, node_id, target_role) VALUES (?1, ?2, 'primary')",
            )
            .bind(app.id)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO app_runtime_states(app_id, node_id, runtime_status, message)
                VALUES (?1, ?2, 'unknown', '等待首次部署')
                ON CONFLICT(app_id, node_id) DO NOTHING
                "#,
            )
            .bind(app.id)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            r#"
            DELETE FROM app_runtime_states
            WHERE app_id = ?1
              AND node_id NOT IN (
                SELECT node_id
                FROM app_targets
                WHERE app_id = ?1
              )
            "#,
        )
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        if let Some(config) = &binary_config_for_metadata {
            upsert_binary_config(&mut tx, app.id, config).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn upload_binary_artifact(
        &self,
        input: UploadBinaryArtifactInput,
    ) -> Result<UploadBinaryArtifactResult, AppError> {
        let app = self.fetch_app_detail(input.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_binary_app(&app)?;
        let result = self
            .register_binary_artifact(RegisterBinaryArtifactInput {
                app,
                artifact_version: input.artifact_version,
                version_code: input.version_code,
                published_at: input.published_at,
                file_name: input.file_name,
                bytes: input.bytes,
                entry_file: input.entry_file,
                source: input.source,
            })
            .await?;
        if result.queued {
            self.enqueue_release_dispatch(result.app_id).await?;
        }
        Ok(result)
    }

    pub async fn upload_release_package(
        &self,
        input: UploadReleasePackageInput,
    ) -> Result<UploadReleasePackageResult, AppError> {
        let app = self.fetch_app_detail(input.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_compose_app(&app)?;
        if app.release_source != "package_upload" {
            return Err(AppError::InvalidInput(
                "当前应用不是版本包发布模式，不能上传版本包".to_owned(),
            ));
        }
        let release_version = normalize_release_id(&input.release_version)?;
        let version_code = match input.version_code {
            Some(version_code) if version_code > 0 => version_code,
            Some(_) => {
                return Err(AppError::InvalidInput(
                    "versionCode 必须是大于 0 的整数".to_owned(),
                ));
            }
            None => version_code_from_release(&release_version).ok_or_else(|| {
                AppError::InvalidInput(
                    "发布版本必须是 vX.Y.Z 格式，或显式传入 versionCode".to_owned(),
                )
            })?,
        };
        let published_at = match normalize_published_at(&input.published_at)? {
            Some(value) => value,
            None => sqlite_now(&self.db).await?,
        };
        let received_at = sqlite_now(&self.db).await?;
        let file_name = required_text(&input.file_name, "请选择版本包文件")?;
        if input.bytes.is_empty() {
            return Err(AppError::InvalidInput("上传文件不能为空".to_owned()));
        }
        let package_kind = artifact_kind_from_file_name(&file_name);
        let uploaded_path = self
            .runtime_fs
            .save_release_package_file(&app.app_key, &release_version, &file_name, &input.bytes)
            .await?;
        if package_kind == "tar_gz" {
            extract_tar_gz(&uploaded_path, &release_version)?;
        }
        let entry_file = if input.entry_file.trim().is_empty() {
            String::new()
        } else {
            normalize_entry_file(&input.entry_file, &file_name, "package")?
        };
        let checksum = sha256_hex(&input.bytes);
        let size_bytes = input.bytes.len() as u64;
        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let auto_queue_release = app.auto_queue_release == 1;
        let release_status = release_status_after_upload(auto_queue_release);
        let package_path = target_work_dir_path(
            &app.work_dir,
            &format!("releases/{release_version}/{file_name}"),
        );
        let extract_dir =
            target_work_dir_path(&app.work_dir, &format!("releases/{release_version}"));

        let mut tx = self.db.begin().await?;
        let config_snapshot = insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id: app.id,
                snapshot_kind: "manual",
                compose_content: &runtime_files.compose_content,
                env_content: &runtime_files.env_content,
                artifact_version: &release_version,
                metadata: runtime_snapshot_metadata(
                    "package_upload",
                    runtime_root.to_string_lossy(),
                    Some(&release_version),
                    Some(&runtime_files.deploy_scripts),
                    None,
                ),
            },
        )
        .await?;
        self.runtime_fs
            .save_release_runtime_metadata(ReleaseRuntimeMetadata {
                app_key: app.app_key.clone(),
                app_id: app.id,
                app_name: app.name.clone(),
                release_version: release_version.clone(),
                version_code,
                package_name: file_name.clone(),
                package_path: package_path.clone(),
                extract_dir: extract_dir.clone(),
                checksum_sha256: checksum.clone(),
                size_bytes,
                published_at: published_at.clone(),
                received_at: received_at.clone(),
                source: artifact_channel_from_source(&input.source).to_owned(),
                config_snapshot_id: Some(config_snapshot.id),
                config_revision_no: Some(config_snapshot.revision_no),
            })
            .await?;
        let release_metadata = render_artifact_metadata(ArtifactMetadataInput {
            source: "package_upload",
            source_detail: upload_source(&input.source),
            unit_name: "",
            uploaded_path: &uploaded_path.to_string_lossy(),
            original_file_name: &file_name,
            entry_file: &entry_file,
            sha256: &checksum,
            size_bytes,
            config_snapshot_id: Some(config_snapshot.id),
            config_revision_no: Some(config_snapshot.revision_no),
        });
        let release_id = upsert_app_release(
            &mut tx,
            app.id,
            &release_version,
            version_code,
            &file_name,
            &package_path,
            &uploaded_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_string_lossy(),
            artifact_channel_from_source(&input.source),
            &checksum,
            size_bytes,
            &published_at,
            release_status,
            STORAGE_PROVIDER_LOCAL,
            "",
            "",
            "",
            &release_metadata,
        )
        .await?;
        let queue_id = if auto_queue_release {
            Some(
                enqueue_app_release(
                    &mut tx,
                    app.id,
                    release_id,
                    config_snapshot.id,
                    upload_source(&input.source),
                    &format!("版本 {} 已入队，等待串行发布", release_version),
                    "queued",
                    None,
                )
                .await?,
            )
        } else {
            None
        };
        sqlx::query(
            r#"
            UPDATE apps
            SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if queue_id.is_some() {
            self.enqueue_release_dispatch(app.id).await?;
        }
        Ok(UploadReleasePackageResult {
            app_id: app.id,
            app_key: app.app_key,
            release_id,
            queue_id,
            release_version,
            version_code,
            package_path,
            package_kind: package_kind.to_owned(),
            published_at,
            config_snapshot_id: config_snapshot.id,
            config_revision_no: config_snapshot.revision_no,
            queued: auto_queue_release,
            publish_status: release_publish_mode_label(auto_queue_release).to_owned(),
            scheduled_publish_at: None,
        })
    }

    pub async fn create_release_package_upload(
        &self,
        input: CreateReleasePackageUploadInput,
    ) -> Result<CreateReleasePackageUploadResult, AppError> {
        let app = self.fetch_app_detail(input.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_compose_app(&app)?;
        if app.release_source != "package_upload" {
            return Err(AppError::InvalidInput(
                "当前应用不是版本包发布模式，不能创建版本包上传会话".to_owned(),
            ));
        }
        let file_name = normalize_package_file_name(&input.file_name)?;
        let parsed = parse_release_package_name_for_service(
            &file_name,
            &app.app_key,
            Some(&input.release_version),
        )
        .map_err(|err| AppError::InvalidInput(err.message()))?;
        let version_code = match input.version_code {
            Some(version_code) if version_code > 0 => version_code,
            Some(_) => {
                return Err(AppError::InvalidInput(
                    "versionCode 必须是大于 0 的整数".to_owned(),
                ));
            }
            None => parsed.version_code,
        };
        let published_at = normalize_published_at(&input.published_at)?.unwrap_or_default();
        let platform_config = self.platform.config().await?;
        if !platform_config.artifact_storage.is_aliyun_oss() {
            return Err(AppError::InvalidInput(
                "平台制品存储未启用阿里云 OSS，不能创建直传上传地址".to_owned(),
            ));
        }
        let oss = &platform_config.artifact_storage.aliyun_oss;
        let object_key = oss.object_key(&app.app_key, &parsed.release_version, &file_name);
        let presigned = oss.presign_upload(&object_key)?;
        let upload_id = generated_release_upload_id();
        let source = upload_source(&input.source).to_owned();
        let metadata = json!({
            "storage_provider": STORAGE_PROVIDER_ALIYUN_OSS,
            "upload_method": presigned.method,
            "upload_content_type": crate::artifact_storage::ARTIFACT_UPLOAD_CONTENT_TYPE,
        })
        .to_string();
        sqlx::query(
            r#"
            INSERT INTO app_release_uploads(
                id,
                app_id,
                release_version,
                version_code,
                file_name,
                object_key,
                bucket,
                endpoint,
                source,
                published_at,
                expires_at,
                metadata
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
        )
        .bind(&upload_id)
        .bind(app.id)
        .bind(&parsed.release_version)
        .bind(version_code)
        .bind(&file_name)
        .bind(&object_key)
        .bind(&oss.bucket)
        .bind(&oss.endpoint)
        .bind(&source)
        .bind(&published_at)
        .bind(&presigned.expires_at)
        .bind(&metadata)
        .execute(&self.db)
        .await?;
        Ok(CreateReleasePackageUploadResult {
            upload_id: upload_id.clone(),
            app_id: app.id,
            app_key: app.app_key.clone(),
            release_version: parsed.release_version,
            version_code,
            file_name,
            object_key,
            bucket: oss.bucket.clone(),
            endpoint: oss.endpoint.clone(),
            upload_url: presigned.url,
            upload_method: presigned.method.to_owned(),
            upload_headers: presigned.headers,
            expires_at: presigned.expires_at,
            complete_path: format!(
                "/api/v1/services/{}/packages/uploads/{}/complete",
                app.app_key, upload_id
            ),
        })
    }

    pub async fn complete_release_package_upload(
        &self,
        input: CompleteReleasePackageUploadInput,
    ) -> Result<UploadReleasePackageResult, AppError> {
        let upload_id = normalize_upload_id(&input.upload_id)?;
        let service_key = normalize_key(&input.service_key)?;
        let checksum = normalize_checksum_sha256(&input.checksum_sha256)?;
        if input.size_bytes <= 0 {
            return Err(AppError::InvalidInput(
                "size_bytes 必须是大于 0 的整数".to_owned(),
            ));
        }
        let upload = sqlx::query_as::<_, AppReleaseUploadRecord>(
            r#"
            SELECT
                id,
                app_id,
                release_version,
                version_code,
                file_name,
                object_key,
                bucket,
                endpoint,
                status,
                source,
                published_at,
                expires_at
            FROM app_release_uploads
            WHERE id = ?1
            "#,
        )
        .bind(&upload_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("上传会话不存在".to_owned()))?;
        if upload.status != "pending" {
            return Err(AppError::Conflict("上传会话已经完成或不可用".to_owned()));
        }
        let app = self.fetch_app_detail(upload.app_id).await?;
        if app.app_key != service_key {
            return Err(AppError::InvalidInput(
                "上传会话不属于当前服务标识".to_owned(),
            ));
        }
        ensure_app_enabled(&app)?;
        self.ensure_compose_app(&app)?;
        if app.release_source != "package_upload" {
            return Err(AppError::InvalidInput(
                "当前应用不是版本包发布模式，不能登记版本包".to_owned(),
            ));
        }
        if upload_session_expired(&upload.expires_at) {
            sqlx::query(
                r#"
                UPDATE app_release_uploads
                SET status = 'expired',
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                WHERE id = ?1
                  AND status = 'pending'
                "#,
            )
            .bind(&upload.id)
            .execute(&self.db)
            .await?;
            return Err(AppError::InvalidInput(
                "上传会话已过期，请重新申请上传地址".to_owned(),
            ));
        }
        let source_text = if input.source.trim().is_empty() {
            upload.source.clone()
        } else {
            input.source.clone()
        };
        let source_detail = upload_source(&source_text).to_owned();
        let published_at = match normalize_published_at(&input.published_at)? {
            Some(value) => value,
            None => match normalize_published_at(&upload.published_at)? {
                Some(value) => value,
                None => sqlite_now(&self.db).await?,
            },
        };
        let received_at = sqlite_now(&self.db).await?;
        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let auto_queue_release = app.auto_queue_release == 1;
        let release_status = release_status_after_upload(auto_queue_release);
        let package_path = target_work_dir_path(
            &app.work_dir,
            &format!("releases/{}/{}", upload.release_version, upload.file_name),
        );
        let extract_dir = target_work_dir_path(
            &app.work_dir,
            &format!("releases/{}", upload.release_version),
        );
        let size_bytes = input.size_bytes as u64;
        let package_kind = artifact_kind_from_file_name(&upload.file_name);

        let mut tx = self.db.begin().await?;
        let config_snapshot = insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id: app.id,
                snapshot_kind: "manual",
                compose_content: &runtime_files.compose_content,
                env_content: &runtime_files.env_content,
                artifact_version: &upload.release_version,
                metadata: runtime_snapshot_metadata(
                    "package_upload",
                    runtime_root.to_string_lossy(),
                    Some(&upload.release_version),
                    Some(&runtime_files.deploy_scripts),
                    None,
                ),
            },
        )
        .await?;
        self.runtime_fs
            .save_release_runtime_metadata(ReleaseRuntimeMetadata {
                app_key: app.app_key.clone(),
                app_id: app.id,
                app_name: app.name.clone(),
                release_version: upload.release_version.clone(),
                version_code: upload.version_code,
                package_name: upload.file_name.clone(),
                package_path: package_path.clone(),
                extract_dir: extract_dir.clone(),
                checksum_sha256: checksum.clone(),
                size_bytes,
                published_at: published_at.clone(),
                received_at: received_at.clone(),
                source: artifact_channel_from_source(&source_detail).to_owned(),
                config_snapshot_id: Some(config_snapshot.id),
                config_revision_no: Some(config_snapshot.revision_no),
            })
            .await?;
        let uploaded_path = format!("oss://{}/{}", upload.bucket, upload.object_key);
        let release_metadata = render_artifact_metadata_with_storage(
            ArtifactMetadataInput {
                source: "package_upload",
                source_detail: &source_detail,
                unit_name: "",
                uploaded_path: &uploaded_path,
                original_file_name: &upload.file_name,
                entry_file: "",
                sha256: &checksum,
                size_bytes,
                config_snapshot_id: Some(config_snapshot.id),
                config_revision_no: Some(config_snapshot.revision_no),
            },
            ArtifactStorageMetadataInput {
                provider: STORAGE_PROVIDER_ALIYUN_OSS,
                bucket: &upload.bucket,
                object_key: &upload.object_key,
                endpoint: &upload.endpoint,
            },
        );
        let release_id = upsert_app_release(
            &mut tx,
            app.id,
            &upload.release_version,
            upload.version_code,
            &upload.file_name,
            &package_path,
            &extract_dir,
            artifact_channel_from_source(&source_detail),
            &checksum,
            size_bytes,
            &published_at,
            release_status,
            STORAGE_PROVIDER_ALIYUN_OSS,
            &upload.bucket,
            &upload.object_key,
            &upload.endpoint,
            &release_metadata,
        )
        .await?;
        let queue_id = if auto_queue_release {
            Some(
                enqueue_app_release(
                    &mut tx,
                    app.id,
                    release_id,
                    config_snapshot.id,
                    &source_detail,
                    &format!("版本 {} 已入队，等待串行发布", upload.release_version),
                    "queued",
                    None,
                )
                .await?,
            )
        } else {
            None
        };
        let update_result = sqlx::query(
            r#"
            UPDATE app_release_uploads
            SET status = 'completed',
                checksum_sha256 = ?2,
                size_bytes = ?3,
                source = ?4,
                published_at = ?5,
                completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
              AND status = 'pending'
            "#,
        )
        .bind(&upload.id)
        .bind(&checksum)
        .bind(input.size_bytes)
        .bind(&source_detail)
        .bind(&published_at)
        .execute(&mut *tx)
        .await?;
        if update_result.rows_affected() == 0 {
            return Err(AppError::Conflict("上传会话已经完成或不可用".to_owned()));
        }
        sqlx::query(
            r#"
            UPDATE apps
            SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        if queue_id.is_some() {
            self.enqueue_release_dispatch(app.id).await?;
        }
        Ok(UploadReleasePackageResult {
            app_id: app.id,
            app_key: app.app_key,
            release_id,
            queue_id,
            release_version: upload.release_version,
            version_code: upload.version_code,
            package_path,
            package_kind: package_kind.to_owned(),
            published_at,
            config_snapshot_id: config_snapshot.id,
            config_revision_no: config_snapshot.revision_no,
            queued: auto_queue_release,
            publish_status: release_publish_mode_label(auto_queue_release).to_owned(),
            scheduled_publish_at: None,
        })
    }

    pub async fn register_binary_artifact_from_task(
        &self,
        input: UploadBinaryArtifactInput,
    ) -> Result<UploadBinaryArtifactResult, AppError> {
        let app = self.fetch_app_detail(input.app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_binary_app(&app)?;
        let result = self
            .register_binary_artifact(RegisterBinaryArtifactInput {
                app,
                artifact_version: input.artifact_version,
                version_code: input.version_code,
                published_at: input.published_at,
                file_name: input.file_name,
                bytes: input.bytes,
                entry_file: input.entry_file,
                source: input.source,
            })
            .await?;
        if result.queued {
            self.enqueue_release_dispatch(result.app_id).await?;
        }
        Ok(result)
    }

    async fn register_binary_artifact(
        &self,
        input: RegisterBinaryArtifactInput,
    ) -> Result<UploadBinaryArtifactResult, AppError> {
        let app = input.app;
        let artifact_version = normalize_release_id(&input.artifact_version)?;
        let version_code = match input.version_code {
            Some(version_code) if version_code > 0 => version_code,
            Some(_) => {
                return Err(AppError::InvalidInput(
                    "versionCode 必须是大于 0 的整数".to_owned(),
                ));
            }
            None => version_code_from_release(&artifact_version).ok_or_else(|| {
                AppError::InvalidInput(
                    "发布版本必须是 vX.Y.Z 格式，或显式传入 versionCode".to_owned(),
                )
            })?,
        };
        let published_at = match normalize_published_at(&input.published_at)? {
            Some(value) => value,
            None => sqlite_now(&self.db).await?,
        };
        let file_name = required_text(&input.file_name, "请选择二进制版本包文件")?;
        if input.bytes.is_empty() {
            return Err(AppError::InvalidInput("上传文件不能为空".to_owned()));
        }
        let current_config = self.load_binary_config(&app).await?;
        let artifact_kind = artifact_kind_from_file_name(&file_name);
        let uploaded_path = self
            .runtime_fs
            .save_binary_release_file(&app.app_key, &artifact_version, &file_name, &input.bytes)
            .await?;
        let checksum = sha256_hex(&input.bytes);
        let size_bytes = input.bytes.len() as u64;
        let entry_file = normalize_entry_file(&input.entry_file, &file_name, artifact_kind)?;
        let local_artifact_path = if artifact_kind == "tar_gz" {
            extract_tar_gz(&uploaded_path, &artifact_version)?;
            uploaded_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(&entry_file)
        } else {
            uploaded_path.clone()
        };
        if !local_artifact_path.is_file() {
            return Err(AppError::InvalidInput(format!(
                "入口文件不存在: {}",
                local_artifact_path.to_string_lossy()
            )));
        }
        let target_artifact_path = target_work_dir_path(
            &app.work_dir,
            &format!("releases/{artifact_version}/{entry_file}"),
        );
        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let mut release_config = current_config.clone();
        release_config.artifact_version = artifact_version.clone();
        release_config.artifact_path = target_artifact_path.clone();
        if release_config.working_dir.trim().is_empty() {
            release_config.working_dir = app.work_dir.clone();
        }
        if release_config.unit_name.trim().is_empty() {
            release_config.unit_name = format!("easy-deploy-{}.service", app.app_key);
        }
        if release_config.service_user.trim().is_empty() {
            release_config.service_user = "deploy".to_owned();
        }
        let upload_metadata = render_artifact_metadata(ArtifactMetadataInput {
            source: "upload",
            source_detail: upload_source(&input.source),
            unit_name: &release_config.unit_name,
            uploaded_path: &uploaded_path.to_string_lossy(),
            original_file_name: &file_name,
            entry_file: &entry_file,
            sha256: &checksum,
            size_bytes,
            config_snapshot_id: None,
            config_revision_no: None,
        });

        let mut tx = self.db.begin().await?;
        upsert_binary_config(&mut tx, app.id, &release_config).await?;
        sqlx::query(
            r#"
            INSERT INTO binary_artifacts(
                app_id,
                version,
                version_code,
                artifact_path,
                artifact_kind,
                status,
                published_at,
                metadata
            )
            VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7)
            ON CONFLICT(app_id, version) DO UPDATE SET
                version_code = excluded.version_code,
                artifact_path = excluded.artifact_path,
                artifact_kind = excluded.artifact_kind,
                status = 'active',
                published_at = excluded.published_at,
                metadata = excluded.metadata
            "#,
        )
        .bind(app.id)
        .bind(&artifact_version)
        .bind(version_code)
        .bind(&target_artifact_path)
        .bind(artifact_kind)
        .bind(&published_at)
        .bind(&upload_metadata)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE binary_artifacts
            SET status = 'registered'
            WHERE app_id = ?1
              AND version != ?2
              AND status = 'active'
            "#,
        )
        .bind(app.id)
        .bind(&artifact_version)
        .execute(&mut *tx)
        .await?;
        let platform_config = self.platform.config().await?;
        let pruned_releases = prune_uploaded_binary_releases(
            &mut tx,
            app.id,
            &artifact_version,
            platform_config.uploaded_binary_releases_to_keep,
        )
        .await?;
        let config_snapshot = insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id: app.id,
                snapshot_kind: "manual",
                compose_content: &runtime_files.compose_content,
                env_content: &release_config.env_content,
                artifact_version: &artifact_version,
                metadata: runtime_snapshot_metadata(
                    "binary_upload",
                    runtime_root.to_string_lossy(),
                    Some(&artifact_version),
                    Some(&runtime_files.deploy_scripts),
                    Some(&release_config),
                ),
            },
        )
        .await?;
        let auto_queue_release = app.auto_queue_release == 1;
        let release_status = release_status_after_upload(auto_queue_release);
        let release_metadata = render_artifact_metadata(ArtifactMetadataInput {
            source: "upload",
            source_detail: upload_source(&input.source),
            unit_name: &release_config.unit_name,
            uploaded_path: &uploaded_path.to_string_lossy(),
            original_file_name: &file_name,
            entry_file: &entry_file,
            sha256: &checksum,
            size_bytes,
            config_snapshot_id: Some(config_snapshot.id),
            config_revision_no: Some(config_snapshot.revision_no),
        });
        let release_id = upsert_app_release(
            &mut tx,
            app.id,
            &artifact_version,
            version_code,
            &file_name,
            &target_artifact_path,
            &uploaded_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_string_lossy(),
            artifact_channel_from_source(&input.source),
            &checksum,
            size_bytes,
            &published_at,
            release_status,
            STORAGE_PROVIDER_LOCAL,
            "",
            "",
            "",
            &release_metadata,
        )
        .await?;
        let queue_id = if auto_queue_release {
            Some(
                enqueue_app_release(
                    &mut tx,
                    app.id,
                    release_id,
                    config_snapshot.id,
                    upload_source(&input.source),
                    &format!("版本 {} 已入队，等待串行发布", artifact_version),
                    "queued",
                    None,
                )
                .await?,
            )
        } else {
            None
        };
        sqlx::query(
            r#"
            UPDATE apps
            SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        cleanup_pruned_binary_release_dirs(&runtime_root, &pruned_releases)?;
        Ok(UploadBinaryArtifactResult {
            app_id: app.id,
            app_key: app.app_key,
            release_id,
            queue_id,
            artifact_version,
            version_code,
            artifact_path: target_artifact_path,
            artifact_kind: artifact_kind.to_owned(),
            published_at,
            config_snapshot_id: config_snapshot.id,
            config_revision_no: config_snapshot.revision_no,
            queued: auto_queue_release,
            publish_status: release_publish_mode_label(auto_queue_release).to_owned(),
            scheduled_publish_at: None,
        })
    }

    pub async fn deploy_binary_artifact(
        &self,
        app_id: i64,
        artifact_id: i64,
        actor: &str,
    ) -> Result<BinaryReleaseDeployResult, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        self.ensure_binary_app(&app)?;
        ensure_has_enabled_targets(&self.target_node_metadata_for_app(app.id).await?)?;
        let artifact = sqlx::query_as::<_, BinaryArtifactItem>(
            r#"
            SELECT
                id,
                version,
                version_code,
                artifact_path,
                artifact_kind,
                status,
                metadata,
                published_at,
                created_at
            FROM binary_artifacts
            WHERE id = ?1
              AND app_id = ?2
            "#,
        )
        .bind(artifact_id)
        .bind(app.id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("二进制发布版本不存在".to_owned()))?;
        if artifact.status == "disabled" {
            return Err(AppError::InvalidInput("该发布版本已禁用".to_owned()));
        }
        let mut config = self.load_binary_config(&app).await?;
        config.artifact_version = artifact.version.clone();
        config.artifact_path = artifact.artifact_path.clone();
        if config.working_dir.trim().is_empty() {
            config.working_dir = app.work_dir.clone();
        }
        if config.unit_name.trim().is_empty() {
            config.unit_name = format!("easy-deploy-{}.service", app.app_key);
        }
        if config.service_user.trim().is_empty() {
            config.service_user = "deploy".to_owned();
        }

        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let target_nodes = self.target_node_metadata_for_app(app.id).await?;
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let metadata_content = render_runtime_metadata(
            &app,
            target_nodes
                .iter()
                .map(|node| TargetNodeMetadata {
                    node_key: node.node_key.clone(),
                    name: node.name.clone(),
                })
                .collect(),
            &runtime_root.to_string_lossy(),
            Some(&config),
        );
        self.runtime_fs
            .save_app_runtime_files(&app.app_key, "", &config.env_content, &metadata_content)
            .await?;
        self.runtime_fs
            .save_binary_runtime_files(to_binary_runtime_config(
                app.id,
                &app.app_key,
                &app.name,
                &config,
            ))
            .await?;

        let mut tx = self.db.begin().await?;
        upsert_binary_config(&mut tx, app.id, &config).await?;
        sqlx::query(
            r#"
            UPDATE binary_artifacts
            SET status = CASE WHEN id = ?2 THEN 'active' ELSE 'registered' END,
                metadata = CASE WHEN id = ?2 THEN ?3 ELSE metadata END,
                artifact_path = CASE WHEN id = ?2 THEN ?4 ELSE artifact_path END
            WHERE app_id = ?1
              AND status != 'disabled'
            "#,
        )
        .bind(app.id)
        .bind(artifact.id)
        .bind(&artifact.metadata)
        .bind(&artifact.artifact_path)
        .execute(&mut *tx)
        .await?;
        let config_snapshot = insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id: app.id,
                snapshot_kind: "manual",
                compose_content: &runtime_files.compose_content,
                env_content: &config.env_content,
                artifact_version: &artifact.version,
                metadata: runtime_snapshot_metadata(
                    "binary_release_deploy",
                    runtime_root.to_string_lossy(),
                    Some(&artifact.version),
                    Some(&runtime_files.deploy_scripts),
                    Some(&config),
                ),
            },
        )
        .await?;
        tx.commit().await?;
        let version = artifact.version.clone();
        let task_id = self
            .create_binary_task_for_config(
                app,
                config,
                BinaryTaskAction::Restart,
                actor,
                "部署发布版本并重启二进制",
                Some(config_snapshot),
            )
            .await?;
        Ok(BinaryReleaseDeployResult { task_id, version })
    }

    pub async fn restore_config_snapshot(
        &self,
        app_id: i64,
        snapshot_id: i64,
    ) -> Result<(), AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        let snapshot = sqlx::query_as::<_, AppConfigSnapshotItem>(
            r#"
            SELECT
                id,
                revision_no,
                snapshot_kind,
                compose_content,
                env_content,
                artifact_version,
                config_hash,
                metadata,
                created_at
            FROM app_config_snapshots
            WHERE id = ?1
              AND app_id = ?2
            "#,
        )
        .bind(snapshot_id)
        .bind(app.id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("配置快照不存在".to_owned()))?;

        let current_binary_config = self.load_binary_config(&app).await?;
        let snapshot_artifact_version = snapshot_artifact_version(&snapshot);
        let snapshot_artifact = if app.app_type == "binary" && !snapshot_artifact_version.is_empty()
        {
            Some(
                self.binary_artifact_by_version(app.id, &snapshot_artifact_version)
                    .await?
                    .ok_or_else(|| {
                        AppError::InvalidInput(format!(
                            "配置快照绑定的发布版本 {snapshot_artifact_version} 不存在，无法恢复"
                        ))
                    })?,
            )
        } else {
            None
        };
        if let Some(artifact) = &snapshot_artifact
            && artifact.status == "disabled"
        {
            return Err(AppError::InvalidInput(format!(
                "配置快照绑定的发布版本 {} 已清理，无法恢复",
                artifact.version
            )));
        }
        let binary_config = if app.app_type == "binary" {
            Some(binary_config_from_snapshot(
                &app,
                &snapshot,
                &current_binary_config,
                snapshot_artifact.as_ref(),
            ))
        } else {
            None
        };
        let compose_content = if app.app_type == "compose" {
            normalize_compose_content(&snapshot.compose_content, &app.app_key)?
        } else {
            String::new()
        };
        let env_content = binary_config
            .as_ref()
            .map(|config| config.env_content.clone())
            .unwrap_or_else(|| normalize_env_content(&snapshot.env_content));
        let deploy_scripts = deploy_scripts_from_snapshot_metadata(&snapshot.metadata);
        let target_nodes = self.target_node_metadata_for_app(app.id).await?;
        let runtime_root = self.runtime_fs.app_root(&app.app_key)?;
        let metadata_content = render_runtime_metadata(
            &app,
            target_nodes
                .iter()
                .map(|node| TargetNodeMetadata {
                    node_key: node.node_key.clone(),
                    name: node.name.clone(),
                })
                .collect(),
            &runtime_root.to_string_lossy(),
            binary_config.as_ref(),
        );
        self.runtime_fs
            .save_app_runtime_files_with_scripts(
                &app.app_key,
                &compose_content,
                &env_content,
                &metadata_content,
                &deploy_scripts,
            )
            .await?;
        if app.app_type == "binary" {
            self.runtime_fs
                .save_binary_runtime_files(to_binary_runtime_config(
                    app.id,
                    &app.app_key,
                    &app.name,
                    binary_config
                        .as_ref()
                        .ok_or_else(|| AppError::Internal("二进制配置恢复失败".to_owned()))?,
                ))
                .await?;
        }

        let mut tx = self.db.begin().await?;
        if let Some(config) = &binary_config {
            upsert_binary_config(&mut tx, app.id, config).await?;
            if let Some(artifact) = &snapshot_artifact {
                sqlx::query(
                    r#"
                    UPDATE binary_artifacts
                    SET status = CASE WHEN id = ?2 THEN 'active' ELSE 'registered' END
                    WHERE app_id = ?1
                      AND status != 'disabled'
                    "#,
                )
                .bind(app.id)
                .bind(artifact.id)
                .execute(&mut *tx)
                .await?;
            }
        }
        let artifact_version = binary_config
            .as_ref()
            .map(|config| config.artifact_version.as_str())
            .unwrap_or("");
        insert_runtime_config_snapshot(
            &mut tx,
            RuntimeConfigSnapshotInput {
                app_id: app.id,
                snapshot_kind: "manual",
                compose_content: &compose_content,
                env_content: &env_content,
                artifact_version,
                metadata: runtime_snapshot_metadata(
                    "restore",
                    runtime_root.to_string_lossy(),
                    Some(&format!(
                        "snapshot-{}-{}",
                        snapshot.id, snapshot.snapshot_kind
                    )),
                    Some(&deploy_scripts),
                    binary_config.as_ref(),
                ),
            },
        )
        .await?;
        sqlx::query(
            r#"
            UPDATE apps
            SET updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(app.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn compose_config(&self, app_id: i64) -> Result<ComposeCommandOutput, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        self.ensure_compose_app(&app)?;
        let work_dir = self.runtime_fs.app_root(&app.app_key)?;
        self.compose.config(work_dir).await.map_err(AppError::from)
    }

    pub async fn compose_logs(&self, app_id: i64) -> Result<ComposeCommandOutput, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_compose_app(&app)?;
        let work_dir = self.runtime_fs.app_root(&app.app_key)?;
        self.compose.logs(work_dir).await.map_err(AppError::from)
    }

    pub async fn deploy_strategy(&self, app_id: i64) -> Result<String, AppError> {
        self.fetch_app_detail(app_id)
            .await
            .map(|app| app.deploy_strategy)
    }

    pub async fn compose_service_logs(
        &self,
        app_id: i64,
        service_name: &str,
        node_id: Option<i64>,
        tail_lines: u16,
    ) -> Result<ServiceLogOutput, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        self.ensure_compose_app(&app)?;
        let runtime_files = self.runtime_fs.load_app_config(&app.app_key).await?;
        let service_exists = parse_compose_services(&runtime_files.compose_content)?
            .iter()
            .any(|service| service.name == service_name);
        if !service_exists {
            return Err(AppError::InvalidInput("Compose 服务不存在".to_owned()));
        }
        let target_nodes = self.target_node_metadata_for_app(app.id).await?;
        let runtime_states = self.list_app_runtime_states(app.id).await?;
        let node = select_service_log_node(&target_nodes, node_id, "Compose 应用未绑定目标节点")?;
        let command_output = match node.node_type.as_str() {
            "local" => self
                .compose
                .service_logs_with_tail(runtime_files.root_dir, service_name, tail_lines)
                .await
                .map_err(AppError::from)?,
            "ssh" => {
                let target = node.ssh_target()?;
                let remote_work_dir = compose_node_deploy_work_dir_for_app(&app, node);
                self.systemd
                    .ssh_executor()
                    .compose_service_logs_with_tail(
                        &target,
                        runtime_files.root_dir,
                        &remote_work_dir,
                        service_name,
                        tail_lines,
                    )
                    .await
                    .map_err(AppError::from)?
            }
            _ => Err(AppError::InvalidInput(format!(
                "节点 {} 的类型 {} 不支持 Compose 日志",
                node.name, node.node_type
            )))?,
        };
        Ok(ServiceLogOutput {
            command_output,
            node: service_target_node_item(node, &runtime_states),
            target_nodes: service_target_node_items(&target_nodes, &runtime_states),
        })
    }

    pub async fn binary_service_logs(
        &self,
        app_id: i64,
        service_name: &str,
        node_id: Option<i64>,
        tail_lines: u16,
    ) -> Result<ServiceLogOutput, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        self.ensure_binary_app(&app)?;
        if service_name != app.app_key {
            return Err(AppError::InvalidInput("二进制服务不存在".to_owned()));
        }
        let config = self.load_binary_config(&app).await?;
        let runtime_work_dir = self.runtime_fs.app_root(&app.app_key)?;
        let target_nodes = self.target_node_metadata_for_app(app.id).await?;
        let runtime_states = self.list_app_runtime_states(app.id).await?;
        let node = select_service_log_node(&target_nodes, node_id, "二进制应用未绑定目标节点")?;
        let command_output = match node.node_type.as_str() {
            "local" => {
                let work_dir = binary_command_work_dir(
                    &binary_node_deploy_work_dir_for_app(&app, node),
                    &runtime_work_dir,
                );
                self.systemd
                    .logs_with_tail(work_dir, &config.unit_name, tail_lines)
                    .await
                    .map_err(AppError::from)?
            }
            "ssh" => {
                let target = node.ssh_target()?;
                self.systemd
                    .ssh_executor()
                    .logs_with_tail(&target, runtime_work_dir, &config.unit_name, tail_lines)
                    .await
                    .map_err(AppError::from)?
            }
            _ => Err(AppError::InvalidInput(format!(
                "节点 {} 的类型 {} 不支持二进制日志",
                node.name, node.node_type
            )))?,
        };
        Ok(ServiceLogOutput {
            command_output,
            node: service_target_node_item(node, &runtime_states),
            target_nodes: service_target_node_items(&target_nodes, &runtime_states),
        })
    }

    pub async fn run_compose_task(
        &self,
        app_id: i64,
        action: ComposeTaskAction,
        actor: &str,
    ) -> Result<i64, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        self.ensure_compose_app(&app)?;
        ensure_has_enabled_targets(&self.target_node_metadata_for_app(app.id).await?)?;
        let task_id = self
            .tasks
            .create_task(CreateTaskInput {
                task_kind: action.task_kind().to_owned(),
                title: format!("{} {}", action.title_prefix(), app.name),
                app_id: Some(app.id),
                release_id: None,
                node_id: None,
                created_by: actor.to_owned(),
            })
            .await?;
        let work_dir = self.runtime_fs.app_root(&app.app_key)?;
        if !work_dir.is_dir() {
            self.tasks
                .fail_task(task_id, "Compose 工作目录不存在")
                .await?;
            return Err(AppError::InvalidInput("Compose 工作目录不存在".to_owned()));
        }
        let deploy_strategy = parse_deploy_strategy(&app.deploy_strategy);
        let compose_strategy = load_app_compose_strategy(&self.db, app.id).await?;
        let config_snapshot =
            self.latest_config_snapshot(app.id)
                .await?
                .unwrap_or(RuntimeConfigSnapshotRecord {
                    id: 0,
                    revision_no: 0,
                });
        self.tasks
            .append_log(task_id, "system", "任务已加入后台部署队列")
            .await?;
        if config_snapshot.revision_no > 0 {
            self.tasks
                .append_log(
                    task_id,
                    "system",
                    &format!("运行配置版本: config#{}", config_snapshot.revision_no),
                )
                .await?;
        }
        self.tasks
            .append_log(
                task_id,
                "system",
                &format!("部署策略: {}", deploy_strategy.label()),
            )
            .await?;
        update_runtime_states_in_db(&RuntimeStatesUpdate {
            db: &self.db,
            app_id: app.id,
            runtime_status: "deploying",
            service_count: None,
            active_version: None,
            message: "任务已加入后台部署队列",
            task_id: Some(task_id),
            touch_deploy_time: false,
        })
        .await?;
        if let Err(err) = self
            .compose_queue
            .enqueue(ComposeTaskJob {
                task_id,
                app_id: app.id,
                release_id: None,
                queue_id: None,
                app_key: app.app_key,
                app_name: app.name,
                environment: app.environment,
                compose_strategy,
                release_version: None,
                release_package_name: None,
                release_checksum_sha256: None,
                release_size_bytes: None,
                release_storage_provider: None,
                release_storage_bucket: None,
                release_storage_object_key: None,
                release_storage_endpoint: None,
                config_snapshot_id: (config_snapshot.id > 0).then_some(config_snapshot.id),
                config_revision_no: config_snapshot.revision_no,
                deploy_strategy,
                action,
            })
            .await
        {
            self.tasks.fail_task(task_id, err.message()).await?;
            update_runtime_states_in_db(&RuntimeStatesUpdate {
                db: &self.db,
                app_id: app.id,
                runtime_status: "unhealthy",
                service_count: None,
                active_version: None,
                message: err.message(),
                task_id: Some(task_id),
                touch_deploy_time: false,
            })
            .await?;
            return Err(err);
        }
        Ok(task_id)
    }

    pub async fn retry_compose_task(&self, task_id: i64, actor: &str) -> Result<i64, AppError> {
        let task = self.tasks.task_detail(task_id).await?;
        if task.status != "failed" {
            return Err(AppError::InvalidInput(
                "只有失败的 Compose 任务可以重试".to_owned(),
            ));
        }
        let action = ComposeTaskAction::from_task_kind(&task.task_kind).ok_or_else(|| {
            AppError::InvalidInput("当前任务不是可重试的 Compose 部署任务".to_owned())
        })?;
        let app_id = task
            .app_id
            .ok_or_else(|| AppError::InvalidInput("任务未关联应用，无法重试".to_owned()))?;
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.run_compose_task(app_id, action, actor).await
    }

    pub async fn run_binary_task(
        &self,
        app_id: i64,
        action: BinaryTaskAction,
        actor: &str,
    ) -> Result<i64, AppError> {
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        self.ensure_binary_app(&app)?;
        ensure_has_enabled_targets(&self.target_node_metadata_for_app(app.id).await?)?;
        let config = self.load_binary_config(&app).await?;
        if config.artifact_version.is_empty() || config.artifact_path.is_empty() {
            return Err(AppError::InvalidInput(
                "请先保存二进制发布版本和部署文件路径".to_owned(),
            ));
        }
        let task_id = self
            .tasks
            .create_task(CreateTaskInput {
                task_kind: action.task_kind().to_owned(),
                title: format!("{} {}", action.title_prefix(), app.name),
                app_id: Some(app.id),
                release_id: None,
                node_id: None,
                created_by: actor.to_owned(),
            })
            .await?;
        let work_dir = self.runtime_fs.app_root(&app.app_key)?;
        if !work_dir.is_dir() {
            self.tasks
                .fail_task(task_id, "二进制工作目录不存在")
                .await?;
            return Err(AppError::InvalidInput("二进制工作目录不存在".to_owned()));
        }
        let deploy_strategy = parse_deploy_strategy(&app.deploy_strategy);
        let config_snapshot =
            self.latest_config_snapshot(app.id)
                .await?
                .unwrap_or(RuntimeConfigSnapshotRecord {
                    id: 0,
                    revision_no: 0,
                });
        self.tasks
            .append_log(task_id, "system", "任务已加入后台二进制部署队列")
            .await?;
        self.tasks
            .append_log(
                task_id,
                "system",
                &format!("部署策略: {}", deploy_strategy.label()),
            )
            .await?;
        self.tasks
            .append_log(
                task_id,
                "system",
                &format!("二进制发布版本: {}", config.artifact_version),
            )
            .await?;
        if config_snapshot.revision_no > 0 {
            self.tasks
                .append_log(
                    task_id,
                    "system",
                    &format!("运行配置版本: config#{}", config_snapshot.revision_no),
                )
                .await?;
        }
        update_runtime_states_in_db(&RuntimeStatesUpdate {
            db: &self.db,
            app_id: app.id,
            runtime_status: "deploying",
            service_count: None,
            active_version: None,
            message: "任务已加入后台二进制部署队列",
            task_id: Some(task_id),
            touch_deploy_time: false,
        })
        .await?;
        if let Err(err) = self
            .binary_queue
            .enqueue(BinaryTaskJob {
                task_id,
                app_id: app.id,
                release_id: None,
                queue_id: None,
                app_key: app.app_key,
                deploy_work_dir: app.work_dir,
                unit_name: config.unit_name,
                artifact_version: config.artifact_version,
                artifact_path: config.artifact_path,
                config_snapshot_id: (config_snapshot.id > 0).then_some(config_snapshot.id),
                config_revision_no: config_snapshot.revision_no,
                release_strategy: config.release_strategy,
                active_slot: config.active_slot,
                base_port: config.base_port,
                standby_port: config.standby_port,
                proxy_enabled: config.proxy_enabled == 1,
                proxy_kind: config.proxy_kind,
                proxy_domain: config.proxy_domain,
                proxy_config_path: config.proxy_config_path,
                deploy_strategy,
                action,
            })
            .await
        {
            self.tasks.fail_task(task_id, err.message()).await?;
            update_runtime_states_in_db(&RuntimeStatesUpdate {
                db: &self.db,
                app_id: app.id,
                runtime_status: "unhealthy",
                service_count: None,
                active_version: None,
                message: err.message(),
                task_id: Some(task_id),
                touch_deploy_time: false,
            })
            .await?;
            return Err(err);
        }
        Ok(task_id)
    }

    async fn create_binary_task_for_config(
        &self,
        app: AppDetailItem,
        config: BinaryConfigItem,
        action: BinaryTaskAction,
        actor: &str,
        title_prefix: &str,
        config_snapshot: Option<RuntimeConfigSnapshotRecord>,
    ) -> Result<i64, AppError> {
        ensure_app_enabled(&app)?;
        self.ensure_no_active_deploy_task(app.id).await?;
        let task_id = self
            .tasks
            .create_task(CreateTaskInput {
                task_kind: action.task_kind().to_owned(),
                title: format!("{title_prefix} {}", app.name),
                app_id: Some(app.id),
                release_id: None,
                node_id: None,
                created_by: actor.to_owned(),
            })
            .await?;
        let work_dir = self.runtime_fs.app_root(&app.app_key)?;
        if !work_dir.is_dir() {
            self.tasks
                .fail_task(task_id, "二进制工作目录不存在")
                .await?;
            return Err(AppError::InvalidInput("二进制工作目录不存在".to_owned()));
        }
        let deploy_strategy = parse_deploy_strategy(&app.deploy_strategy);
        self.tasks
            .append_log(task_id, "system", "任务已加入后台二进制部署队列")
            .await?;
        self.tasks
            .append_log(
                task_id,
                "system",
                &format!("部署策略: {}", deploy_strategy.label()),
            )
            .await?;
        self.tasks
            .append_log(
                task_id,
                "system",
                &format!("二进制发布版本: {}", config.artifact_version),
            )
            .await?;
        update_runtime_states_in_db(&RuntimeStatesUpdate {
            db: &self.db,
            app_id: app.id,
            runtime_status: "deploying",
            service_count: None,
            active_version: None,
            message: "任务已加入后台二进制部署队列",
            task_id: Some(task_id),
            touch_deploy_time: false,
        })
        .await?;
        let config_snapshot =
            match config_snapshot {
                Some(snapshot) => snapshot,
                None => self.latest_config_snapshot(app.id).await?.unwrap_or(
                    RuntimeConfigSnapshotRecord {
                        id: 0,
                        revision_no: 0,
                    },
                ),
            };
        if let Err(err) = self
            .binary_queue
            .enqueue(BinaryTaskJob {
                task_id,
                app_id: app.id,
                release_id: None,
                queue_id: None,
                app_key: app.app_key,
                deploy_work_dir: app.work_dir,
                unit_name: config.unit_name,
                artifact_version: config.artifact_version,
                artifact_path: config.artifact_path,
                config_snapshot_id: (config_snapshot.id > 0).then_some(config_snapshot.id),
                config_revision_no: config_snapshot.revision_no,
                release_strategy: config.release_strategy,
                active_slot: config.active_slot,
                base_port: config.base_port,
                standby_port: config.standby_port,
                proxy_enabled: config.proxy_enabled == 1,
                proxy_kind: config.proxy_kind,
                proxy_domain: config.proxy_domain,
                proxy_config_path: config.proxy_config_path,
                deploy_strategy,
                action,
            })
            .await
        {
            self.tasks.fail_task(task_id, err.message()).await?;
            update_runtime_states_in_db(&RuntimeStatesUpdate {
                db: &self.db,
                app_id: app.id,
                runtime_status: "unhealthy",
                service_count: None,
                active_version: None,
                message: err.message(),
                task_id: Some(task_id),
                touch_deploy_time: false,
            })
            .await?;
            return Err(err);
        }
        Ok(task_id)
    }

    pub async fn retry_binary_task(&self, task_id: i64, actor: &str) -> Result<i64, AppError> {
        let task = self.tasks.task_detail(task_id).await?;
        if task.status != "failed" {
            return Err(AppError::InvalidInput(
                "只有失败的二进制任务可以重试".to_owned(),
            ));
        }
        let action = BinaryTaskAction::from_task_kind(&task.task_kind).ok_or_else(|| {
            AppError::InvalidInput("当前任务不是可重试的二进制部署任务".to_owned())
        })?;
        let app_id = task
            .app_id
            .ok_or_else(|| AppError::InvalidInput("任务未关联应用，无法重试".to_owned()))?;
        let app = self.fetch_app_detail(app_id).await?;
        ensure_app_enabled(&app)?;
        self.run_binary_task(app_id, action, actor).await
    }

    pub async fn set_app_enabled(
        &self,
        app_id: i64,
        enabled: bool,
    ) -> Result<AppStatusChange, AppError> {
        let status = if enabled { "ready" } else { "disabled" };
        let app = self.fetch_app_detail(app_id).await?;
        if app.status == status {
            return Ok(AppStatusChange {
                app_id: app.id,
                app_name: app.name,
                previous_status: app.status.clone(),
                status: app.status,
            });
        }
        self.ensure_no_active_deploy_task(app.id).await?;
        update_app_status_in_db(&self.db, app.id, status).await?;
        Ok(AppStatusChange {
            app_id: app.id,
            app_name: app.name,
            previous_status: app.status,
            status: status.to_owned(),
        })
    }

    pub async fn set_app_status(
        &self,
        app_id: i64,
        status: &str,
    ) -> Result<AppStatusChange, AppError> {
        match status {
            "disabled" => self.set_app_enabled(app_id, false).await,
            "ready" => self.set_app_enabled(app_id, true).await,
            _ => Err(AppError::InvalidInput("应用只支持启用或停用".to_owned())),
        }
    }

    fn ensure_compose_app(&self, app: &AppDetailItem) -> Result<(), AppError> {
        if app.app_type == "compose" {
            Ok(())
        } else {
            Err(AppError::InvalidInput(
                "当前应用不是 Docker Compose 应用".to_owned(),
            ))
        }
    }

    fn ensure_binary_app(&self, app: &AppDetailItem) -> Result<(), AppError> {
        if app.app_type == "binary" {
            Ok(())
        } else {
            Err(AppError::InvalidInput("当前应用不是二进制应用".to_owned()))
        }
    }

    async fn fetch_app_detail(&self, app_id: i64) -> Result<AppDetailItem, AppError> {
        sqlx::query_as::<_, AppDetailItem>(
            r#"
            SELECT
                a.id,
                a.app_key,
                a.name,
                a.description,
                a.environment,
                a.app_type,
                a.deploy_mode,
                a.deploy_strategy,
                a.release_source,
                a.compose_strategy,
                a.auto_queue_release,
                a.work_dir,
                a.status,
                group_concat(n.name, '、') AS target_names,
                COUNT(n.id) AS target_count,
                a.created_at,
                a.updated_at
            FROM apps a
            LEFT JOIN app_targets t ON t.app_id = a.id
            LEFT JOIN nodes n ON n.id = t.node_id
            WHERE a.id = ?1
            GROUP BY a.id
            "#,
        )
        .bind(app_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| AppError::InvalidInput("应用不存在".to_owned()))
    }

    async fn target_node_metadata(&self, node_ids: &[i64]) -> Result<Vec<AppTargetNode>, AppError> {
        let mut nodes = Vec::new();
        for node_id in dedupe_ids(node_ids) {
            let node = sqlx::query_as::<_, AppTargetNode>(
                r#"
                SELECT
                    n.id,
                    n.node_key,
                    n.name,
                    n.node_type,
                    n.status,
                    n.address,
                    n.ssh_port,
                    n.ssh_user,
                    cred.private_key_path AS credential_private_key_path,
                    n.work_dir,
                    COALESCE(c.caddy_available, 0) AS caddy_available,
                    COALESCE(c.nginx_available, 0) AS nginx_available
                FROM nodes n
                LEFT JOIN node_credentials cred ON cred.id = n.credential_id
                LEFT JOIN node_capabilities c ON c.node_id = n.id
                WHERE n.id = ?1
                  AND n.status != 'disabled'
                "#,
            )
            .bind(node_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| AppError::InvalidInput("目标节点不存在或已禁用".to_owned()))?;
            nodes.push(node);
        }
        Ok(nodes)
    }

    async fn target_node_metadata_for_app(
        &self,
        app_id: i64,
    ) -> Result<Vec<AppTargetNode>, AppError> {
        sqlx::query_as::<_, AppTargetNode>(
            r#"
            SELECT
                n.id,
                n.node_key,
                n.name,
                n.node_type,
                n.status,
                n.address,
                n.ssh_port,
                n.ssh_user,
                cred.private_key_path AS credential_private_key_path,
                n.work_dir,
                COALESCE(c.caddy_available, 0) AS caddy_available,
                COALESCE(c.nginx_available, 0) AS nginx_available
            FROM nodes n
            JOIN app_targets t ON t.node_id = n.id
            LEFT JOIN node_credentials cred ON cred.id = n.credential_id
            LEFT JOIN node_capabilities c ON c.node_id = n.id
            WHERE t.app_id = ?1
              AND n.status != 'disabled'
            ORDER BY n.id
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
        .map_err(AppError::from)
    }

    async fn ensure_no_active_deploy_task(&self, app_id: i64) -> Result<(), AppError> {
        if let Some(task) = self.tasks.active_app_task(app_id).await? {
            return Err(AppError::Conflict(format!(
                "该应用已有活跃部署任务 #{} {}（{}，{}），请等待结束或取消后再提交",
                task.id,
                task.title,
                active_task_status_label(&task.status),
                task_phase_label(&task.phase)
            )));
        }
        let deploying_nodes = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(1)
            FROM app_runtime_states
            WHERE app_id = ?1
              AND runtime_status = 'deploying'
            "#,
        )
        .bind(app_id)
        .fetch_one(&self.db)
        .await?;
        if deploying_nodes > 0 {
            return Err(AppError::Conflict(
                "应用正在部署中，请等待当前任务结束后再提交".to_owned(),
            ));
        }
        Ok(())
    }
}

fn ensure_app_enabled(app: &AppDetailItem) -> Result<(), AppError> {
    if app.status == "disabled" {
        Err(AppError::InvalidInput(
            "应用已停用，不能执行变更操作".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn ensure_has_enabled_targets(nodes: &[AppTargetNode]) -> Result<(), AppError> {
    if nodes.is_empty() {
        Err(AppError::InvalidInput(
            "应用没有可用目标节点，请先启用节点或调整目标节点".to_owned(),
        ))
    } else {
        Ok(())
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

fn select_service_log_node<'a>(
    nodes: &'a [AppTargetNode],
    node_id: Option<i64>,
    empty_message: &str,
) -> Result<&'a AppTargetNode, AppError> {
    let Some(node_id) = node_id else {
        return nodes
            .first()
            .ok_or_else(|| AppError::InvalidInput(empty_message.to_owned()));
    };
    nodes
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| AppError::InvalidInput("目标节点不属于当前应用或已禁用".to_owned()))
}

fn service_target_node_items(
    nodes: &[AppTargetNode],
    states: &[AppRuntimeStateItem],
) -> Vec<ServiceTargetNodeItem> {
    nodes
        .iter()
        .map(|node| service_target_node_item(node, states))
        .collect()
}

fn service_target_node_item(
    node: &AppTargetNode,
    states: &[AppRuntimeStateItem],
) -> ServiceTargetNodeItem {
    let state = states.iter().find(|state| state.node_id == node.id);
    ServiceTargetNodeItem {
        id: node.id,
        name: node.name.clone(),
        node_key: node.node_key.clone(),
        runtime_status: state
            .map(|state| state.runtime_status.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        active_version: state
            .map(|state| state.active_version.clone())
            .unwrap_or_default(),
        service_count: state.map(|state| state.service_count).unwrap_or(0),
        message: state
            .map(|state| state.message.clone())
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| "等待首次部署".to_owned()),
        last_task_id: state.and_then(|state| state.last_task_id),
        last_task_status: state.and_then(|state| state.last_task_status.clone()),
        last_task_kind: state.and_then(|state| state.last_task_kind.clone()),
        last_deploy_at: state.and_then(|state| state.last_deploy_at.clone()),
        updated_at: state
            .map(|state| state.updated_at.clone())
            .unwrap_or_else(|| "未检查".to_owned()),
    }
}

#[derive(Clone, Debug)]
struct ServiceRuntimeOverview {
    status: String,
    summary: String,
    active_version: String,
    latest_message: String,
    latest_checked_at: String,
}

fn service_runtime_overview(states: &[AppRuntimeStateItem]) -> ServiceRuntimeOverview {
    if states.is_empty() {
        return ServiceRuntimeOverview {
            status: "unknown".to_owned(),
            summary: "暂无节点运行记录".to_owned(),
            active_version: String::new(),
            latest_message: "暂无健康检查结果".to_owned(),
            latest_checked_at: "未检查".to_owned(),
        };
    }

    let healthy = states
        .iter()
        .filter(|state| state.runtime_status == "healthy")
        .count();
    let unhealthy = states
        .iter()
        .filter(|state| state.runtime_status == "unhealthy")
        .count();
    let deploying = states
        .iter()
        .filter(|state| state.runtime_status == "deploying")
        .count();
    let stopped = states
        .iter()
        .filter(|state| state.runtime_status == "stopped")
        .count();
    let known = healthy + unhealthy + deploying + stopped;
    let unknown = states.len().saturating_sub(known);
    let status = if deploying > 0 {
        "deploying"
    } else if unhealthy > 0 {
        "unhealthy"
    } else if healthy > 0 && healthy == states.len() {
        "healthy"
    } else if stopped > 0 && stopped == states.len() {
        "stopped"
    } else {
        "unknown"
    };
    let active_version = common_active_version(states);
    let (latest_message, latest_checked_at) = latest_runtime_message(states);

    ServiceRuntimeOverview {
        status: status.to_owned(),
        summary: format_runtime_summary(healthy, unhealthy, deploying, stopped, unknown),
        active_version,
        latest_message,
        latest_checked_at,
    }
}

fn latest_runtime_message(states: &[AppRuntimeStateItem]) -> (String, String) {
    let Some(state) = states
        .iter()
        .filter(|state| state.last_deploy_at.is_some() || state.runtime_status != "unknown")
        .max_by_key(|state| state.last_deploy_at.as_deref().unwrap_or(&state.updated_at))
    else {
        return ("暂无健康检查结果".to_owned(), "未检查".to_owned());
    };
    let message = if state.message.trim().is_empty() {
        "暂无健康检查结果".to_owned()
    } else {
        state.message.clone()
    };
    let checked_at = state
        .last_deploy_at
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| state.updated_at.clone());
    (message, checked_at)
}

fn health_check_detail_text(
    config: &HealthCheckConfig,
    binary_config: Option<&BinaryConfigItem>,
) -> String {
    match config.kind {
        HealthCheckKind::None => "不执行健康检查".to_owned(),
        HealthCheckKind::Http => format!(
            "{} · {} 秒 · HTTP {}",
            display_health_endpoint(&config.endpoint, "未配置地址"),
            config.timeout_secs,
            config.expected_status
        ),
        HealthCheckKind::Tcp => format!(
            "{} · {} 秒",
            display_health_endpoint(&config.endpoint, "未配置地址"),
            config.timeout_secs
        ),
        HealthCheckKind::ComposeRunning => format!("容器运行状态 · {} 秒", config.timeout_secs),
        HealthCheckKind::SystemdActive => {
            let endpoint = binary_config
                .filter(|binary| binary.release_strategy == "blue_green")
                .map(|binary| {
                    binary_blue_green_unit_name(
                        &binary.unit_name,
                        normalized_slot(&binary.active_slot),
                    )
                })
                .unwrap_or_else(|| config.endpoint.clone());
            format!(
                "{} · {} 秒",
                display_health_endpoint(&endpoint, "未配置 unit"),
                config.timeout_secs
            )
        }
    }
}

fn display_health_endpoint(value: &str, fallback: &'static str) -> String {
    if value.trim().is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

fn common_active_version(states: &[AppRuntimeStateItem]) -> String {
    let mut versions = states
        .iter()
        .map(|state| state.active_version.trim())
        .filter(|version| !version.is_empty());
    let Some(first) = versions.next() else {
        return String::new();
    };
    if versions.all(|version| version == first) {
        first.to_owned()
    } else {
        "多版本".to_owned()
    }
}

fn format_runtime_summary(
    healthy: usize,
    unhealthy: usize,
    deploying: usize,
    stopped: usize,
    unknown: usize,
) -> String {
    let parts = [
        (healthy, "健康"),
        (unhealthy, "异常"),
        (deploying, "部署中"),
        (stopped, "已停止"),
        (unknown, "未知"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{label} {count}"))
    .collect::<Vec<_>>();

    if parts.is_empty() {
        "暂无节点运行记录".to_owned()
    } else {
        parts.join(" · ")
    }
}

async fn update_app_status_in_db(
    db: &SqlitePool,
    app_id: i64,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE apps
        SET status = ?2,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(app_id)
    .bind(status)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeTaskAction {
    Up,
    Down,
    Restart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryTaskAction {
    Restart,
    Stop,
}

impl ComposeTaskAction {
    fn from_task_kind(value: &str) -> Option<Self> {
        match value {
            "compose.up" => Some(Self::Up),
            "compose.down" => Some(Self::Down),
            "compose.restart" => Some(Self::Restart),
            _ => None,
        }
    }

    fn task_kind(self) -> &'static str {
        match self {
            Self::Up => "compose.up",
            Self::Down => "compose.down",
            Self::Restart => "compose.restart",
        }
    }

    fn deploy_action(self) -> &'static str {
        match self {
            Self::Up => "compose_up",
            Self::Down => "compose_down",
            Self::Restart => "compose_restart",
        }
    }

    fn title_prefix(self) -> &'static str {
        match self {
            Self::Up => "部署",
            Self::Down => "停止",
            Self::Restart => "重启",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Up => "启动",
            Self::Down => "停止",
            Self::Restart => "重启",
        }
    }

    fn runs_health_check(self) -> bool {
        matches!(self, Self::Up | Self::Restart)
    }

    fn runtime_status(self, command_success: bool, deployment_status: &str) -> &'static str {
        if !command_success || deployment_status == "failed" {
            "unhealthy"
        } else {
            match self {
                Self::Down => "stopped",
                Self::Up | Self::Restart => "healthy",
            }
        }
    }
}

fn compose_action_command_label(action: ComposeTaskAction) -> &'static str {
    match action {
        ComposeTaskAction::Up => "docker compose up -d --remove-orphans",
        ComposeTaskAction::Down => "docker compose down",
        ComposeTaskAction::Restart => "docker compose restart",
    }
}

impl BinaryTaskAction {
    pub fn from_task_kind(value: &str) -> Option<Self> {
        match value {
            "binary.restart" => Some(Self::Restart),
            "binary.stop" => Some(Self::Stop),
            _ => None,
        }
    }

    fn task_kind(self) -> &'static str {
        match self {
            Self::Restart => "binary.restart",
            Self::Stop => "binary.stop",
        }
    }

    fn deploy_action(self) -> &'static str {
        match self {
            Self::Restart => "binary_restart",
            Self::Stop => "binary_stop",
        }
    }

    fn title_prefix(self) -> &'static str {
        match self {
            Self::Restart => "重启二进制",
            Self::Stop => "停止二进制",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Restart => "重启",
            Self::Stop => "停止",
        }
    }

    fn runs_health_check(self) -> bool {
        matches!(self, Self::Restart)
    }

    fn syncs_runtime_files(self) -> bool {
        matches!(self, Self::Restart)
    }

    fn runtime_status(self, command_success: bool, deployment_status: &str) -> &'static str {
        if !command_success || deployment_status == "failed" {
            "unhealthy"
        } else {
            match self {
                Self::Restart => "healthy",
                Self::Stop => "stopped",
            }
        }
    }
}

fn binary_action_command_label(action: BinaryTaskAction, unit_name: &str) -> String {
    match action {
        BinaryTaskAction::Restart => format!("systemctl restart {unit_name}"),
        BinaryTaskAction::Stop => format!("systemctl stop {unit_name}"),
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct AppTargetNode {
    id: i64,
    node_key: String,
    name: String,
    node_type: String,
    status: String,
    address: String,
    ssh_port: i64,
    ssh_user: String,
    credential_private_key_path: Option<String>,
    work_dir: String,
    caddy_available: i64,
    nginx_available: i64,
}

impl AppTargetNode {
    fn ssh_target(&self) -> Result<SshTarget, AppError> {
        let identity_file = self
            .credential_private_key_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        SshTarget::new(&self.ssh_user, &self.address, self.ssh_port)
            .map(|target| target.with_identity_file(identity_file))
            .map_err(AppError::from)
    }
}

fn normalize_key(value: &str) -> Result<String, AppError> {
    let key = value.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err(AppError::InvalidInput("请输入应用标识".to_owned()));
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(AppError::InvalidInput(
            "应用标识仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    Ok(key)
}

fn normalize_app_type(value: &str) -> Result<String, AppError> {
    let app_type = value.trim().to_ascii_lowercase();
    match app_type.as_str() {
        "compose" | "binary" => Ok(app_type),
        _ => Err(AppError::InvalidInput("应用类型不支持".to_owned())),
    }
}

fn normalize_app_environment(value: &str) -> Result<String, AppError> {
    let environment = value.trim().to_ascii_lowercase();
    if environment.is_empty() {
        return Ok("test".to_owned());
    }
    match environment.as_str() {
        "production" | "prod" => Ok("production".to_owned()),
        "test" | "testing" => Ok("test".to_owned()),
        _ => Err(AppError::InvalidInput(
            "应用环境仅支持 production 或 test".to_owned(),
        )),
    }
}

fn normalize_binary_config(
    input: NormalizeBinaryConfigInput<'_>,
) -> Result<BinaryConfigItem, AppError> {
    let artifact_version = input.artifact_version.trim().to_owned();
    let artifact_path = input.artifact_path.trim().to_owned();
    let unit_name = normalize_unit_name(input.unit_name, input.app_key)?;
    let service_user = input.service_user.trim();
    let service_user = if service_user.is_empty() {
        "deploy"
    } else {
        service_user
    };
    if !service_user
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(AppError::InvalidInput(
            "运行用户仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    let release_strategy = normalize_binary_release_strategy(input.release_strategy)?;
    let active_slot = normalize_binary_slot(input.active_slot)?;
    let base_port = normalize_binary_port(input.base_port, "主槽端口")?;
    let standby_port = normalize_binary_port(input.standby_port, "备用槽端口")?;
    let (proxy_enabled, proxy_kind, proxy_domain, proxy_config_path) =
        normalize_binary_proxy_config(
            input.proxy_enabled,
            input.proxy_kind,
            input.proxy_domain,
            input.proxy_config_path,
            &release_strategy,
            input.app_key,
        )?;

    Ok(BinaryConfigItem {
        service_name: input.app_key.to_owned(),
        artifact_version,
        artifact_path,
        exec_args: input.exec_args.trim().to_owned(),
        working_dir: input.work_dir.trim().to_owned(),
        service_user: service_user.to_owned(),
        unit_name,
        release_strategy,
        active_slot,
        base_port,
        standby_port,
        proxy_enabled,
        proxy_kind,
        proxy_domain,
        proxy_config_path,
        env_content: normalize_env_content(input.env_content),
    })
}

fn normalize_binary_proxy_config(
    enabled: bool,
    kind: &str,
    domain: &str,
    config_path: &str,
    release_strategy: &str,
    app_key: &str,
) -> Result<(i64, String, String, String), AppError> {
    let kind = kind.trim().to_ascii_lowercase();
    let kind = if kind.is_empty() {
        "none".to_owned()
    } else {
        kind
    };
    let domain = domain.trim().to_owned();
    let config_path = config_path.trim().replace('\\', "/");

    if !enabled {
        if !matches!(kind.as_str(), "none" | "caddy" | "nginx") {
            return Err(AppError::InvalidInput(
                "反向代理类型仅支持 none、caddy 或 nginx".to_owned(),
            ));
        }
        return Ok((0, kind, domain, config_path));
    }

    if release_strategy != "blue_green" {
        return Err(AppError::InvalidInput(
            "反向代理切流仅支持 Blue/Green 发布策略".to_owned(),
        ));
    }
    if !matches!(kind.as_str(), "caddy" | "nginx") {
        return Err(AppError::InvalidInput(
            "启用反向代理切流时请选择 caddy 或 nginx".to_owned(),
        ));
    }
    if !is_valid_proxy_domain(&domain) {
        return Err(AppError::InvalidInput(
            "反向代理域名仅支持合法域名、IPv4 或 localhost".to_owned(),
        ));
    }
    let config_path = if config_path.is_empty() {
        default_proxy_config_path(&kind, app_key)
    } else {
        normalize_proxy_config_path(&config_path)?
    };
    Ok((1, kind, domain, config_path))
}

fn default_proxy_config_path(kind: &str, app_key: &str) -> String {
    match kind {
        "nginx" => format!("/etc/nginx/conf.d/{app_key}.conf"),
        _ => format!("/etc/caddy/Caddyfile.d/{app_key}.caddy"),
    }
}

fn normalize_proxy_config_path(value: &str) -> Result<String, AppError> {
    if !value.starts_with('/') && !is_windows_absolute_path(value) {
        return Err(AppError::InvalidInput(
            "反向代理配置路径必须是绝对路径".to_owned(),
        ));
    }
    if value.contains("//")
        || value.split('/').any(|part| part == "." || part == "..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '.' | '-' | '_' | '@'))
    {
        return Err(AppError::InvalidInput(
            "反向代理配置路径仅支持字母、数字、斜线、盘符、点、短横线、下划线和 @".to_owned(),
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn is_windows_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn is_valid_proxy_domain(value: &str) -> bool {
    if value.eq_ignore_ascii_case("localhost") || value.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    if value.len() > 253 || value.is_empty() || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    value.split('.').all(|part| {
        !part.is_empty()
            && part.len() <= 63
            && !part.starts_with('-')
            && !part.ends_with('-')
            && part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    })
}

fn normalize_binary_release_strategy(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok("restart".to_owned());
    }
    match value {
        "restart" | "blue_green" => Ok(value.to_owned()),
        _ => Err(AppError::InvalidInput(
            "二进制发布策略仅支持 restart 或 blue_green".to_owned(),
        )),
    }
}

fn normalize_binary_slot(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok("blue".to_owned());
    }
    match value {
        "blue" | "green" => Ok(value.to_owned()),
        _ => Err(AppError::InvalidInput(
            "当前流量槽位仅支持 blue 或 green".to_owned(),
        )),
    }
}

fn normalize_binary_port(port: i64, label: &str) -> Result<i64, AppError> {
    if port == 0 || (1..=65535).contains(&port) {
        Ok(port)
    } else {
        Err(AppError::InvalidInput(format!(
            "{label}需要在 1 到 65535 之间，留空时使用 0"
        )))
    }
}

fn normalize_unit_name(value: &str, app_key: &str) -> Result<String, AppError> {
    let unit_name = if value.trim().is_empty() {
        format!("easy-deploy-{app_key}.service")
    } else {
        value.trim().to_owned()
    };
    if !unit_name.ends_with(".service") {
        return Err(AppError::InvalidInput(
            "systemd unit 必须以 .service 结尾".to_owned(),
        ));
    }
    if !unit_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@'))
    {
        return Err(AppError::InvalidInput(
            "systemd unit 仅支持字母、数字、短横线、下划线、点和 @".to_owned(),
        ));
    }
    Ok(unit_name)
}

fn default_binary_config_for_app(app_key: &str, work_dir: &str) -> BinaryConfigItem {
    BinaryConfigItem {
        service_name: app_key.to_owned(),
        artifact_version: String::new(),
        artifact_path: String::new(),
        exec_args: String::new(),
        working_dir: work_dir.to_owned(),
        service_user: "deploy".to_owned(),
        unit_name: format!("easy-deploy-{app_key}.service"),
        release_strategy: "restart".to_owned(),
        active_slot: "blue".to_owned(),
        base_port: 0,
        standby_port: 0,
        proxy_enabled: 0,
        proxy_kind: "none".to_owned(),
        proxy_domain: String::new(),
        proxy_config_path: String::new(),
        env_content: String::new(),
    }
}

fn required_text(value: &str, message: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() {
        Err(AppError::InvalidInput(message.to_owned()))
    } else {
        Ok(value.to_owned())
    }
}

fn normalize_package_file_name(value: &str) -> Result<String, AppError> {
    let value = required_text(value, "请填写版本包文件名")?;
    let file_name = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if file_name.is_empty()
        || file_name.contains(char::is_whitespace)
        || !file_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@'))
    {
        return Err(AppError::InvalidInput(
            "版本包文件名仅支持字母、数字、点、短横线、下划线和 @".to_owned(),
        ));
    }
    Ok(file_name)
}

fn normalize_upload_id(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(AppError::InvalidInput("上传会话标识无效".to_owned()));
    }
    Ok(value.to_owned())
}

fn normalize_deploy_work_dir(value: &str) -> Result<String, AppError> {
    let value = required_text(value, "请输入部署目录")?
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    if value.contains('\n') || value.contains('\r') {
        return Err(AppError::InvalidInput("部署目录不能包含换行".to_owned()));
    }
    if !value.starts_with('/') && !value.starts_with('.') {
        return Err(AppError::InvalidInput(
            "部署目录必须使用绝对路径，或使用 . 开头的相对路径".to_owned(),
        ));
    }
    let last_segment = value.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    if matches!(
        last_segment.as_str(),
        "compose.yaml" | "compose.yml" | "docker-compose.yaml" | "docker-compose.yml"
    ) {
        return Err(AppError::InvalidInput(
            "部署目录必须是应用目录，compose.yaml 会由平台固定生成在该目录下".to_owned(),
        ));
    }
    Ok(value)
}

fn normalize_compose_content(value: &str, app_key: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(default_compose_content(app_key));
    }
    let value = strip_top_level_compose_version(value);
    validate_compose_deploy_conventions(&value)?;
    Ok(ensure_trailing_newline(&value))
}

fn validate_compose_deploy_conventions(value: &str) -> Result<(), AppError> {
    let parsed = serde_yaml::from_str::<Value>(value)
        .map_err(|err| AppError::InvalidInput(format!("Compose YAML 解析失败: {err}")))?;
    let Some(services) = parsed.get("services").and_then(Value::as_mapping) else {
        return Err(AppError::InvalidInput(
            "Compose 内容需要包含 services 定义".to_owned(),
        ));
    };
    if services.is_empty() {
        return Err(AppError::InvalidInput(
            "Compose 内容需要至少定义一个 service".to_owned(),
        ));
    }
    validate_compose_volume_sources(services)
}

fn validate_compose_volume_sources(services: &serde_yaml::Mapping) -> Result<(), AppError> {
    for (service_name, service) in services {
        let service_name = service_name.as_str().unwrap_or("unknown");
        let Some(volumes) = service.get("volumes").and_then(Value::as_sequence) else {
            continue;
        };
        for volume in volumes {
            match volume {
                Value::String(value) => {
                    let source = compose_volume_source_from_short_syntax(value);
                    validate_compose_volume_source(service_name, &source)?;
                }
                Value::Mapping(mapping) => {
                    let volume_type = mapping
                        .get(Value::String("type".to_owned()))
                        .and_then(Value::as_str)
                        .unwrap_or("bind");
                    if volume_type == "tmpfs" {
                        continue;
                    }
                    let source = mapping
                        .get(Value::String("source".to_owned()))
                        .or_else(|| mapping.get(Value::String("src".to_owned())))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    validate_compose_volume_source(service_name, source)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn compose_volume_source_from_short_syntax(value: &str) -> String {
    let value = value.trim();
    let Some((source, _target)) = value.split_once(':') else {
        return value.to_owned();
    };
    source.trim().to_owned()
}

fn validate_compose_volume_source(service_name: &str, source: &str) -> Result<(), AppError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(AppError::InvalidInput(format!(
            "Compose service {service_name} 的 volume 缺少宿主机 source，请使用 ./data、./config、./logs 等相对目录"
        )));
    }
    if source.starts_with("./") {
        let relative = source.strip_prefix("./").unwrap_or(source);
        if relative.is_empty()
            || relative
                .split('/')
                .any(|segment| matches!(segment, "" | "." | ".."))
        {
            return Err(AppError::InvalidInput(format!(
                "Compose service {service_name} 的 volume source {source} 必须留在应用目录内"
            )));
        }
        return Ok(());
    }
    if is_allowed_system_bind_mount(source) {
        return Ok(());
    }
    Err(AppError::InvalidInput(format!(
        "Compose service {service_name} 的 volume source {source} 不符合目录约定；持久化目录请放在 compose.yaml 同级目录下，例如 ./data、./config、./logs"
    )))
}

fn is_allowed_system_bind_mount(source: &str) -> bool {
    matches!(source, "/var/run/docker.sock")
}

fn render_runtime_metadata(
    app: &AppDetailItem,
    target_nodes: Vec<TargetNodeMetadata>,
    runtime_root: &str,
    binary: Option<&BinaryConfigItem>,
) -> String {
    let mut output = String::new();
    output.push_str("app_id: ");
    output.push_str(&app.id.to_string());
    output.push('\n');
    push_yaml_string(&mut output, "app_key", &app.app_key);
    push_yaml_string(&mut output, "name", &app.name);
    push_yaml_string(&mut output, "description", &app.description);
    push_yaml_string(&mut output, "environment", &app.environment);
    push_yaml_string(&mut output, "app_type", &app.app_type);
    push_yaml_string(&mut output, "deploy_mode", &app.deploy_mode);
    push_yaml_string(&mut output, "deploy_strategy", &app.deploy_strategy);
    push_yaml_string(&mut output, "release_source", &app.release_source);
    output.push_str("auto_queue_release: ");
    output.push_str(if app.auto_queue_release == 1 {
        "true"
    } else {
        "false"
    });
    output.push('\n');
    push_yaml_string(&mut output, "deploy_work_dir", &app.work_dir);
    push_yaml_string(&mut output, "runtime_root", runtime_root);
    output.push_str("target_nodes:\n");
    for node in target_nodes {
        output.push_str("  - node_key: \"");
        output.push_str(&yaml_escape(&node.node_key));
        output.push_str("\"\n");
        output.push_str("    name: \"");
        output.push_str(&yaml_escape(&node.name));
        output.push_str("\"\n");
    }
    if let Some(binary) = binary {
        output.push_str("binary:\n");
        push_indented_yaml_string(&mut output, "service_name", &binary.service_name, 2);
        push_indented_yaml_string(&mut output, "artifact_version", &binary.artifact_version, 2);
        push_indented_yaml_string(&mut output, "artifact_path", &binary.artifact_path, 2);
        push_indented_yaml_string(&mut output, "exec_args", &binary.exec_args, 2);
        push_indented_yaml_string(&mut output, "working_dir", &binary.working_dir, 2);
        push_indented_yaml_string(&mut output, "service_user", &binary.service_user, 2);
        push_indented_yaml_string(&mut output, "unit_name", &binary.unit_name, 2);
        push_indented_yaml_string(&mut output, "release_strategy", &binary.release_strategy, 2);
        push_indented_yaml_string(&mut output, "active_slot", &binary.active_slot, 2);
        output.push_str("  base_port: ");
        output.push_str(&binary.base_port.to_string());
        output.push('\n');
        output.push_str("  standby_port: ");
        output.push_str(&binary.standby_port.to_string());
        output.push('\n');
        output.push_str("  proxy_enabled: ");
        output.push_str(if binary.proxy_enabled == 1 {
            "true"
        } else {
            "false"
        });
        output.push('\n');
        push_indented_yaml_string(&mut output, "proxy_kind", &binary.proxy_kind, 2);
        push_indented_yaml_string(&mut output, "proxy_domain", &binary.proxy_domain, 2);
        push_indented_yaml_string(
            &mut output,
            "proxy_config_path",
            &binary.proxy_config_path,
            2,
        );
        push_indented_yaml_string(
            &mut output,
            "unit_file",
            &format!(".easy-deploy/systemd/{}", binary.unit_name),
            2,
        );
        push_indented_yaml_string(
            &mut output,
            "env_file",
            &format!(
                ".easy-deploy/systemd/{}",
                binary_unit_env_file_name(&binary.unit_name)
            ),
            2,
        );
        push_indented_yaml_string(
            &mut output,
            "release_file",
            &format!("releases/{}/release.yaml", binary.artifact_version),
            2,
        );
        push_indented_yaml_string(&mut output, "current_release_file", "current", 2);
    }
    output
}

fn push_yaml_string(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": \"");
    output.push_str(&yaml_escape(value));
    output.push_str("\"\n");
}

fn push_indented_yaml_string(output: &mut String, key: &str, value: &str, indent: usize) {
    output.push_str(&" ".repeat(indent));
    push_yaml_string(output, key, value);
}

fn yaml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn to_binary_runtime_metadata(config: &BinaryConfigItem) -> BinaryRuntimeMetadata {
    BinaryRuntimeMetadata {
        service_name: config.service_name.clone(),
        artifact_version: config.artifact_version.clone(),
        artifact_path: config.artifact_path.clone(),
        exec_args: config.exec_args.clone(),
        working_dir: config.working_dir.clone(),
        service_user: config.service_user.clone(),
        unit_name: config.unit_name.clone(),
        release_strategy: config.release_strategy.clone(),
        active_slot: config.active_slot.clone(),
        base_port: config.base_port,
        standby_port: config.standby_port,
        proxy_enabled: config.proxy_enabled == 1,
        proxy_kind: config.proxy_kind.clone(),
        proxy_domain: config.proxy_domain.clone(),
        proxy_config_path: config.proxy_config_path.clone(),
        env_content: config.env_content.clone(),
    }
}

fn to_binary_runtime_config(
    app_id: i64,
    app_key: &str,
    name: &str,
    config: &BinaryConfigItem,
) -> BinaryRuntimeConfig {
    BinaryRuntimeConfig {
        app_key: app_key.to_owned(),
        app_id,
        name: name.to_owned(),
        service_name: config.service_name.clone(),
        artifact_version: config.artifact_version.clone(),
        artifact_path: config.artifact_path.clone(),
        exec_args: config.exec_args.clone(),
        working_dir: config.working_dir.clone(),
        service_user: config.service_user.clone(),
        unit_name: config.unit_name.clone(),
        release_strategy: config.release_strategy.clone(),
        active_slot: config.active_slot.clone(),
        base_port: config.base_port,
        standby_port: config.standby_port,
        proxy_enabled: config.proxy_enabled == 1,
        proxy_kind: config.proxy_kind.clone(),
        proxy_domain: config.proxy_domain.clone(),
        proxy_config_path: config.proxy_config_path.clone(),
        env_content: config.env_content.clone(),
    }
}

fn binary_unit_env_file_name(unit_name: &str) -> String {
    let stem = unit_name.strip_suffix(".service").unwrap_or(unit_name);
    format!("{stem}.env")
}

fn binary_blue_green_unit_name(unit_name: &str, slot: &str) -> String {
    let stem = unit_name.strip_suffix(".service").unwrap_or(unit_name);
    format!("{stem}-{slot}.service")
}

fn binary_blue_green_job_plan_message(job: &BinaryTaskJob) -> String {
    let proxy_plan = if job.proxy_enabled {
        format!(
            "健康检查通过后会切换 {} 反向代理，失败时保留当前槽位并停止备用槽位",
            binary_proxy_kind_label(&job.proxy_kind)
        )
    } else {
        "未启用反向代理切流，成功后只记录新槽位".to_owned()
    };
    format!(
        "Blue/Green 预案: 当前槽位 {}({})，备用槽位 {}({})；本次会启动并检查备用槽位 systemd unit，{}",
        job.active_slot,
        display_port(job.active_port()),
        job.target_slot(),
        display_port(job.target_port()),
        proxy_plan
    )
}

fn standby_slot(active_slot: &str) -> &'static str {
    if active_slot == "green" {
        "blue"
    } else {
        "green"
    }
}

fn normalized_slot(slot: &str) -> &'static str {
    if slot == "green" { "green" } else { "blue" }
}

fn display_port(port: i64) -> String {
    if port > 0 {
        port.to_string()
    } else {
        "未设置端口".to_owned()
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

fn replace_endpoint_port(endpoint: &str, active_port: i64, target_port: i64) -> String {
    if active_port <= 0 || target_port <= 0 || active_port == target_port {
        return endpoint.to_owned();
    }
    if let Ok(mut url) = Url::parse(endpoint) {
        if url.port() == Some(active_port as u16) && url.set_port(Some(target_port as u16)).is_ok()
        {
            return url.to_string();
        }
        return endpoint.to_owned();
    }
    let Some((host, port)) = endpoint.rsplit_once(':') else {
        return endpoint.to_owned();
    };
    if port.parse::<i64>().ok() != Some(active_port) {
        return endpoint.to_owned();
    }
    format!("{host}:{target_port}")
}

struct ArtifactMetadataInput<'a> {
    source: &'a str,
    source_detail: &'a str,
    unit_name: &'a str,
    uploaded_path: &'a str,
    original_file_name: &'a str,
    entry_file: &'a str,
    sha256: &'a str,
    size_bytes: u64,
    config_snapshot_id: Option<i64>,
    config_revision_no: Option<i64>,
}

struct ArtifactStorageMetadataInput<'a> {
    provider: &'a str,
    bucket: &'a str,
    object_key: &'a str,
    endpoint: &'a str,
}

fn artifact_metadata_value_json(input: ArtifactMetadataInput<'_>) -> JsonValue {
    let mut metadata = json!({
        "source": input.source,
        "source_detail": input.source_detail,
        "unit_name": input.unit_name,
        "uploaded_path": input.uploaded_path,
        "original_file_name": input.original_file_name,
        "entry_file": input.entry_file,
        "sha256": input.sha256,
        "size_bytes": input.size_bytes,
    });
    if let Some(snapshot_id) = input.config_snapshot_id {
        metadata["config_snapshot_id"] = json!(snapshot_id);
    }
    if let Some(revision_no) = input.config_revision_no {
        metadata["config_revision_no"] = json!(revision_no);
    }
    metadata
}

fn render_artifact_metadata(input: ArtifactMetadataInput<'_>) -> String {
    artifact_metadata_value_json(input).to_string()
}

fn render_artifact_metadata_with_storage(
    input: ArtifactMetadataInput<'_>,
    storage: ArtifactStorageMetadataInput<'_>,
) -> String {
    let mut metadata = artifact_metadata_value_json(input);
    metadata["storage_provider"] = json!(storage.provider);
    metadata["storage_bucket"] = json!(storage.bucket);
    metadata["storage_object_key"] = json!(storage.object_key);
    metadata["storage_endpoint"] = json!(storage.endpoint);
    metadata.to_string()
}

fn upload_source(source: &str) -> &str {
    let source = source.trim();
    if source.is_empty() { "upload" } else { source }
}

fn artifact_channel_from_source(source: &str) -> &'static str {
    match upload_source(source) {
        "upload" | "web" => "web",
        _ => "openapi",
    }
}

pub fn artifact_metadata_value(metadata: &str, key: &str) -> String {
    serde_json::from_str::<JsonValue>(metadata)
        .ok()
        .and_then(|value| value.get(key).cloned())
        .and_then(|field| match field {
            JsonValue::String(value) => Some(value),
            JsonValue::Number(value) => Some(value.to_string()),
            JsonValue::Bool(value) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn release_metadata_snapshot_id(metadata: &str) -> Option<i64> {
    serde_json::from_str::<JsonValue>(metadata)
        .ok()
        .and_then(|value| value.get("config_snapshot_id").and_then(JsonValue::as_i64))
}

fn release_metadata_with_snapshot(
    metadata: &str,
    snapshot_id: i64,
    revision_no: Option<i64>,
) -> Result<String, AppError> {
    let mut value = if metadata.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<JsonValue>(metadata)
            .map_err(|_| AppError::InvalidInput("发布版本元数据格式损坏".to_owned()))?
    };
    if !value.is_object() {
        value = json!({});
    }
    value["config_snapshot_id"] = json!(snapshot_id);
    if let Some(revision_no) = revision_no {
        value["config_revision_no"] = json!(revision_no);
    }
    Ok(value.to_string())
}

fn normalize_release_id(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(AppError::InvalidInput(
            "二进制版本仅支持字母、数字、短横线、下划线和点".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

pub fn parse_release_package_name(
    file_name: &str,
) -> Result<ParsedReleasePackageName, BinaryPackageNameError> {
    let file_name = file_name
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let file_stem = strip_binary_package_extension(file_name);
    let Some((service_key, version)) = file_stem.rsplit_once("_version_") else {
        return Err(BinaryPackageNameError::InvalidPackageVersionName);
    };
    let service_key = normalize_key(service_key)
        .map_err(|_| BinaryPackageNameError::InvalidPackageVersionName)?;
    let release_version = normalize_package_version(version)
        .ok_or(BinaryPackageNameError::InvalidPackageVersionName)?;
    let version_code = version_code_from_release(&release_version)
        .ok_or(BinaryPackageNameError::InvalidPackageVersionName)?;
    Ok(ParsedReleasePackageName {
        service_key,
        release_version,
        version_code,
    })
}

pub fn parse_release_package_name_for_service(
    file_name: &str,
    expected_service_key: &str,
    explicit_release_version: Option<&str>,
) -> Result<ParsedReleasePackageName, BinaryPackageNameError> {
    let parsed = parse_release_package_name(file_name)?;
    let expected_service_key = normalize_key(expected_service_key)
        .map_err(|_| BinaryPackageNameError::InvalidPackageVersionName)?;
    if parsed.service_key != expected_service_key {
        return Err(BinaryPackageNameError::ServiceKeyMismatch {
            expected: expected_service_key,
            actual: parsed.service_key,
        });
    }
    if let Some(explicit_release_version) = explicit_release_version
        && !explicit_release_version.trim().is_empty()
    {
        let explicit_release_version = normalize_package_version(explicit_release_version)
            .ok_or(BinaryPackageNameError::InvalidPackageVersionName)?;
        if parsed.release_version != explicit_release_version {
            return Err(BinaryPackageNameError::PackageVersionConflict {
                expected: explicit_release_version,
                actual: parsed.release_version,
            });
        }
    }
    Ok(parsed)
}

pub fn parse_binary_package_name(
    file_name: &str,
) -> Result<ParsedBinaryPackageName, BinaryPackageNameError> {
    parse_release_package_name(file_name)
}

pub fn parse_binary_package_name_for_service(
    file_name: &str,
    expected_service_key: &str,
    explicit_release_version: Option<&str>,
) -> Result<ParsedBinaryPackageName, BinaryPackageNameError> {
    parse_release_package_name_for_service(
        file_name,
        expected_service_key,
        explicit_release_version,
    )
}

fn strip_binary_package_extension(file_name: &str) -> &str {
    for extension in [".tar.gz", ".tgz", ".jar", ".zip"] {
        if let Some(stripped) = file_name.strip_suffix(extension) {
            return stripped;
        }
    }
    file_name
}

fn normalize_package_version(version: &str) -> Option<String> {
    let version = version.trim();
    let version = version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
        .replace('_', ".");
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    Some(format!("v{}.{}.{}", parts[0], parts[1], parts[2]))
}

fn version_code_from_release(version: &str) -> Option<i64> {
    let version = normalize_package_version(version)?;
    let version = version.strip_prefix('v').unwrap_or(&version);
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<i64>().ok()?;
    let minor = parts.next()?.parse::<i64>().ok()?;
    let patch = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() || major < 0 || minor < 0 || patch < 0 {
        return None;
    }
    Some(major * 1_000_000 + minor * 1_000 + patch)
}

fn normalize_published_at(value: &str) -> Result<Option<String>, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(
            parsed
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true),
        ));
    }
    for pattern in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(value, pattern) {
            let offset = FixedOffset::east_opt(8 * 60 * 60)
                .ok_or_else(|| AppError::InvalidInput("东八区时区解析失败".to_owned()))?;
            let local = parsed.and_local_timezone(offset).single().ok_or_else(|| {
                AppError::InvalidInput("发布时间格式不正确，请检查日期和时间".to_owned())
            })?;
            return Ok(Some(
                local
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
            ));
        }
    }
    if value.len() < 10
        || value.len() > 64
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | ':' | '.' | 'T' | 'Z' | '+'))
    {
        return Err(AppError::InvalidInput(
            "发布时间格式不正确，请使用 ISO-8601 时间，例如 2026-06-09T10:00:00Z，或页面中的本地时间".to_owned(),
        ));
    }
    Ok(Some(value.to_owned()))
}

#[allow(clippy::too_many_arguments)]
async fn upsert_app_release(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    app_id: i64,
    version: &str,
    version_code: i64,
    package_name: &str,
    package_path: &str,
    extract_dir: &str,
    source: &str,
    checksum_sha256: &str,
    size_bytes: u64,
    published_at: &str,
    status: &str,
    storage_provider: &str,
    storage_bucket: &str,
    storage_object_key: &str,
    storage_endpoint: &str,
    metadata: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO app_releases(
            app_id,
            version,
            version_code,
            package_name,
            package_path,
            extract_dir,
            status,
            source,
            checksum_sha256,
            size_bytes,
            published_at,
            storage_provider,
            storage_bucket,
            storage_object_key,
            storage_endpoint,
            metadata,
            updated_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        ON CONFLICT(app_id, version) DO UPDATE SET
            version_code = excluded.version_code,
            package_name = excluded.package_name,
            package_path = excluded.package_path,
            extract_dir = excluded.extract_dir,
            status = excluded.status,
            source = excluded.source,
            checksum_sha256 = excluded.checksum_sha256,
            size_bytes = excluded.size_bytes,
            published_at = excluded.published_at,
            storage_provider = excluded.storage_provider,
            storage_bucket = excluded.storage_bucket,
            storage_object_key = excluded.storage_object_key,
            storage_endpoint = excluded.storage_endpoint,
            metadata = excluded.metadata,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        RETURNING id
        "#,
    )
    .bind(app_id)
    .bind(version)
    .bind(version_code)
    .bind(package_name)
    .bind(package_path)
    .bind(extract_dir)
    .bind(status)
    .bind(source)
    .bind(checksum_sha256)
    .bind(size_bytes as i64)
    .bind(published_at)
    .bind(storage_provider)
    .bind(storage_bucket)
    .bind(storage_object_key)
    .bind(storage_endpoint)
    .bind(metadata)
    .fetch_one(&mut **tx)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn enqueue_app_release(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    app_id: i64,
    release_id: i64,
    config_snapshot_id: i64,
    triggered_by: &str,
    message: &str,
    status: &str,
    scheduled_publish_at: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let queue_seq = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COALESCE(MAX(queue_seq), 0) + 1
        FROM app_release_queue
        WHERE app_id = ?1
        "#,
    )
    .bind(app_id)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO app_release_queue(
            app_id,
            release_id,
            config_snapshot_id,
            queue_seq,
            status,
            triggered_by,
            message,
            scheduled_publish_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        RETURNING id
        "#,
    )
    .bind(app_id)
    .bind(release_id)
    .bind(config_snapshot_id)
    .bind(queue_seq)
    .bind(status)
    .bind(triggered_by)
    .bind(message)
    .bind(scheduled_publish_at)
    .fetch_one(&mut **tx)
    .await
}

async fn mark_release_queue_running(
    db: &SqlitePool,
    queue_id: i64,
    task_id: i64,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        r#"
        UPDATE app_release_queue
        SET status = 'running',
            task_id = ?2,
            started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
          AND status = 'queued'
        "#,
    )
    .bind(queue_id)
    .bind(task_id)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

async fn finish_release_queue_item(
    db: &SqlitePool,
    queue_id: i64,
    release_id: i64,
    status: &str,
    message: &str,
) -> Result<(), AppError> {
    let release_status = match status {
        "success" => "deployed",
        "failed" => "failed",
        "canceled" => "canceled",
        _ => "failed",
    };
    sqlx::query(
        r#"
        UPDATE app_release_queue
        SET status = ?2,
            message = ?3,
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(queue_id)
    .bind(status)
    .bind(message)
    .execute(db)
    .await?;
    sqlx::query(
        r#"
        UPDATE app_releases
        SET status = ?2,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1
        "#,
    )
    .bind(release_id)
    .bind(release_status)
    .execute(db)
    .await?;
    Ok(())
}

async fn sqlite_now(db: &SqlitePool) -> Result<String, AppError> {
    sqlx::query_scalar::<_, String>("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')")
        .fetch_one(db)
        .await
        .map_err(AppError::from)
}

fn generated_release_upload_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let mut random = [0_u8; 8];
    OsRng.fill_bytes(&mut random);
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("upload-{ts}-{suffix}")
}

fn upload_session_expired(expires_at: &str) -> bool {
    DateTime::parse_from_rfc3339(expires_at)
        .map(|expires_at| expires_at.timestamp() <= unix_timestamp_now())
        .unwrap_or(false)
}

fn unix_timestamp_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn artifact_kind_from_file_name(file_name: &str) -> &'static str {
    if file_name.ends_with(".tar.gz") || file_name.ends_with(".tgz") {
        "tar_gz"
    } else {
        "binary"
    }
}

fn normalize_entry_file(
    entry_file: &str,
    file_name: &str,
    artifact_kind: &str,
) -> Result<String, AppError> {
    let default_entry = if artifact_kind == "tar_gz" {
        ""
    } else {
        file_name
    };
    let value = entry_file.trim();
    let value = if value.is_empty() {
        default_entry
    } else {
        value
    };
    if value.is_empty() {
        return Err(AppError::InvalidInput(
            "tar.gz 版本包需要填写入口文件，例如 bin/server".to_owned(),
        ));
    }
    if value.starts_with('/')
        || value.contains('\\')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(AppError::InvalidInput(
            "入口文件必须是发布目录内的相对路径".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn extract_tar_gz(path: &Path, artifact_version: &str) -> Result<(), AppError> {
    let release_dir = path
        .parent()
        .ok_or_else(|| AppError::Internal("无法定位发布版本目录".to_owned()))?
        .to_path_buf();
    let file = File::open(path).map_err(|err| {
        AppError::Internal(format!(
            "读取 tar.gz 版本包 {} 失败: {err}",
            path.to_string_lossy()
        ))
    })?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let entries = archive.entries().map_err(|err| {
        AppError::InvalidInput(format!("解析 tar.gz 版本包 {artifact_version} 失败: {err}"))
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|err| {
            AppError::InvalidInput(format!("读取 tar.gz 条目 {artifact_version} 失败: {err}"))
        })?;
        let entry_path = entry.path().map_err(|err| {
            AppError::InvalidInput(format!("读取 tar.gz 路径 {artifact_version} 失败: {err}"))
        })?;
        let sanitized = sanitize_archive_path(&entry_path)?;
        let target = release_dir.join(sanitized);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                AppError::Internal(format!(
                    "创建解包目录 {} 失败: {err}",
                    parent.to_string_lossy()
                ))
            })?;
        }
        entry.unpack(&target).map_err(|err| {
            AppError::InvalidInput(format!(
                "解包 tar.gz 条目 {} 失败: {err}",
                target.to_string_lossy()
            ))
        })?;
    }
    Ok(())
}

fn sanitize_archive_path(path: &Path) -> Result<PathBuf, AppError> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => output.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(AppError::InvalidInput(
                    "tar.gz 版本包不能包含绝对路径或上级目录".to_owned(),
                ));
            }
        }
    }
    if output.as_os_str().is_empty() {
        return Err(AppError::InvalidInput("tar.gz 条目路径无效".to_owned()));
    }
    Ok(output)
}

fn default_compose_content(app_key: &str) -> String {
    format!("services:\n  {app_key}:\n    image: nginx:alpine\n    restart: unless-stopped\n")
}

fn normalize_env_content(value: &str) -> String {
    ensure_trailing_newline(value.trim())
}

fn strip_top_level_compose_version(value: &str) -> String {
    let mut output = Vec::new();
    for line in value.lines() {
        let trimmed = line.trim_start();
        let is_top_level = line.len() == trimmed.len();
        if is_top_level && is_compose_version_line(trimmed) {
            continue;
        }
        output.push(line);
    }
    output.join("\n").trim().to_owned()
}

fn is_compose_version_line(value: &str) -> bool {
    let Some((key, rest)) = value.split_once(':') else {
        return false;
    };
    key.trim() == "version" && !rest.trim().is_empty()
}

fn strip_common_error_prefix(value: &str) -> &str {
    value
        .strip_prefix("Error response from daemon: ")
        .or_else(|| value.strip_prefix("error during connect: "))
        .or_else(|| value.strip_prefix("ERROR: "))
        .or_else(|| value.strip_prefix("Error: "))
        .unwrap_or(value)
}

fn ensure_trailing_newline(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else if value.ends_with('\n') {
        value.to_owned()
    } else {
        format!("{value}\n")
    }
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn dedupe_ids(ids: &[i64]) -> Vec<i64> {
    let mut deduped = Vec::new();
    for id in ids {
        if !deduped.contains(id) {
            deduped.push(*id);
        }
    }
    deduped
}

fn dedupe_strings(values: &[String]) -> Vec<String> {
    let mut deduped = Vec::new();
    for value in values {
        let value = value.trim();
        if !value.is_empty() && !deduped.iter().any(|item: &String| item == value) {
            deduped.push(value.to_owned());
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{
        deploy::{CommandResult, CommandRunner, CommandSpec, DynCommandRunner},
        runtimefs::RELEASE_META_FILE_NAME,
    };
    use async_trait::async_trait;
    use sqlx::sqlite::SqliteConnectOptions;
    use tempfile::{TempDir, tempdir};

    use super::*;

    #[derive(Default)]
    struct NoopCommandRunner;

    #[async_trait]
    impl CommandRunner for NoopCommandRunner {
        async fn run(&self, _spec: CommandSpec) -> Result<CommandResult, DeployError> {
            Ok(CommandResult {
                status_code: Some(0),
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
            })
        }
    }

    #[derive(Default)]
    struct RecordingCommandRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl CommandRunner for RecordingCommandRunner {
        async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
            self.specs.lock().expect("lock command specs").push(spec);
            Ok(CommandResult {
                status_code: Some(0),
                stdout: "ok\n".to_owned(),
                stderr: String::new(),
            })
        }
    }

    #[derive(Default)]
    struct ComposeUpFailureRunner {
        specs: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl CommandRunner for ComposeUpFailureRunner {
        async fn run(&self, spec: CommandSpec) -> Result<CommandResult, DeployError> {
            let should_fail = spec.program == "docker"
                && spec.args.windows(2).any(|window| {
                    window[0] == "compose"
                        && matches!(window.get(1).map(String::as_str), Some("up" | "restart"))
                });
            self.specs.lock().expect("lock command specs").push(spec);
            if should_fail {
                Ok(CommandResult {
                    status_code: Some(1),
                    stdout: String::new(),
                    stderr: "compose up failed".to_owned(),
                })
            } else {
                Ok(CommandResult {
                    status_code: Some(0),
                    stdout: "ok\n".to_owned(),
                    stderr: String::new(),
                })
            }
        }
    }

    async fn task_service() -> TaskService {
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
        TaskService::new(db)
    }

    async fn app_service() -> (AppService, SqlitePool, TempDir) {
        app_service_with_runner(Arc::new(NoopCommandRunner)).await
    }

    async fn app_service_with_runner(
        runner: DynCommandRunner,
    ) -> (AppService, SqlitePool, TempDir) {
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
        let data_dir = tempdir().expect("create app data dir");
        let tasks = TaskService::new(db.clone());
        let platform = PlatformConfigService::new(db.clone());
        let apps = AppService::new(
            db.clone(),
            RuntimeFs::new(data_dir.path()),
            ComposeExecutor::new(runner.clone()),
            SystemdExecutor::new(runner),
            tasks,
            platform,
        );
        (apps, db, data_dir)
    }

    async fn create_manual_compose_app(apps: &AppService, app_key: &str) -> i64 {
        apps.create_app(CreateAppInput {
            app_key: app_key.to_owned(),
            name: format!("{app_key} app"),
            description: String::new(),
            environment: "test".to_owned(),
            app_type: "compose".to_owned(),
            deploy_strategy: "rolling_stop_on_failure".to_owned(),
            release_source: "package_upload".to_owned(),
            auto_queue_release: false,
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
        .expect("create manual compose app")
    }

    async fn create_auto_compose_app(apps: &AppService, app_key: &str) -> i64 {
        apps.create_app(CreateAppInput {
            app_key: app_key.to_owned(),
            name: format!("{app_key} app"),
            description: String::new(),
            environment: "test".to_owned(),
            app_type: "compose".to_owned(),
            deploy_strategy: "rolling_stop_on_failure".to_owned(),
            release_source: "package_upload".to_owned(),
            auto_queue_release: true,
            work_dir: format!("/opt/easy-deploy/apps/{app_key}"),
            target_node_ids: vec![1],
            compose_content: "services:\n  app:\n    image: nginx:alpine\n".to_owned(),
            env_content: "RUST_LOG=info".to_owned(),
            deploy_scripts: DeployScriptSet {
                pre_deploy: "echo pre".to_owned(),
                deploy: String::new(),
                post_deploy: "echo post".to_owned(),
                switch_traffic: "echo switch".to_owned(),
                cleanup: "echo cleanup".to_owned(),
            },
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
        .expect("create auto compose app")
    }

    async fn create_ssh_target_node(db: &SqlitePool, data_dir: &TempDir) -> i64 {
        let identity_file = data_dir.path().join("id_ed25519");
        fs::write(&identity_file, "private key").expect("write identity file");
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
            VALUES ('ssh-test-key', 'SSH 测试密钥', 'ssh-ed25519 AAAA', ?1, 'SHA256:ssh-test', 'active')
            "#,
        )
        .bind(identity_file.to_string_lossy().to_string())
        .execute(db)
        .await
        .expect("insert ssh credential")
        .last_insert_rowid();

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
            VALUES (
                'ssh-prod-a',
                'SSH 生产节点 A',
                'ssh',
                '10.0.2.11',
                22,
                'deploy',
                ?1,
                '/opt/easy-deploy/apps',
                'prod',
                'ssh',
                'online',
                'available'
            )
            "#,
        )
        .bind(credential_id)
        .execute(db)
        .await
        .expect("insert ssh node")
        .last_insert_rowid()
    }

    async fn upload_manual_release(
        apps: &AppService,
        app_id: i64,
        version: &str,
    ) -> UploadReleasePackageResult {
        apps.upload_release_package(UploadReleasePackageInput {
            app_id,
            release_version: version.to_owned(),
            version_code: None,
            published_at: "2026-06-23T10:00:00Z".to_owned(),
            file_name: format!("package-{version}.bin"),
            bytes: format!("release {version}").into_bytes(),
            entry_file: String::new(),
            source: "web".to_owned(),
        })
        .await
        .expect("upload manual release")
    }

    async fn wait_for_release_queue_status(
        apps: &AppService,
        release_id: i64,
        expected_status: &str,
    ) -> AppReleaseQueueItem {
        let mut last_status = String::new();
        for _ in 0..200 {
            let queue = apps
                .list_app_release_queue()
                .await
                .expect("list release queue while waiting");
            if let Some(item) = queue.into_iter().find(|item| item.release_id == release_id) {
                last_status = item.status.clone();
                if item.status == expected_status {
                    return item;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "release {release_id} did not reach status {expected_status}, last status: {last_status}"
        );
    }

    #[tokio::test]
    async fn command_output_can_be_attached_to_task_step() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "部署 Redis".to_owned(),
                app_id: None,
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");
        let step_id = start_task_step(
            &tasks,
            task_id,
            None,
            "compose.action",
            "启动 Compose 服务",
            "docker compose up -d",
        )
        .await
        .expect("start task step");
        let output = ComposeCommandOutput {
            command: "docker compose up -d".to_owned(),
            success: true,
            status_code: Some(0),
            output: "Container redis Started".to_owned(),
        };

        append_step_command_output(&tasks, task_id, step_id, &output)
            .await
            .expect("append step command output");
        finish_task_step(&tasks, task_id, step_id, true, Some(0), "启动完成")
            .await
            .expect("finish step");

        let logs = tasks.task_logs(task_id).await.expect("task logs");
        assert!(logs.iter().any(|log| {
            log.step_id == Some(step_id)
                && log.stream == "system"
                && log.content.contains("docker compose up -d")
        }));
        assert!(logs.iter().any(|log| {
            log.step_id == Some(step_id)
                && log.stream == "combined"
                && log.content.contains("redis")
        }));
    }

    #[tokio::test]
    async fn manual_release_upload_can_be_scheduled_and_canceled() {
        let (apps, _db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "orders-api").await;
        let upload = upload_manual_release(&apps, app_id, "v1.2.3").await;

        assert!(!upload.queued);
        assert_eq!(upload.queue_id, None);
        assert_eq!(upload.publish_status, release_publish_mode_label(false));
        let releases = apps.list_app_releases().await.expect("list releases");
        let release = releases
            .iter()
            .find(|item| item.id == upload.release_id)
            .expect("uploaded release");
        assert_eq!(release.status, "received");
        assert_eq!(release.scheduled_publish_at, None);

        let scheduled_at = apps
            .schedule_release_publish(upload.release_id, "2030-01-02T03:04:05Z")
            .await
            .expect("schedule release");
        assert_eq!(scheduled_at, "2030-01-02T03:04:05Z");

        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list release queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].release_id, upload.release_id);
        assert_eq!(queue[0].status, "scheduled");
        assert_eq!(
            queue[0].scheduled_publish_at.as_deref(),
            Some("2030-01-02T03:04:05Z")
        );

        let releases = apps
            .list_app_releases()
            .await
            .expect("list scheduled releases");
        let release = releases
            .iter()
            .find(|item| item.id == upload.release_id)
            .expect("scheduled release");
        assert_eq!(release.status, "queued");
        assert_eq!(
            release.scheduled_publish_at.as_deref(),
            Some("2030-01-02T03:04:05Z")
        );

        apps.cancel_scheduled_release(upload.release_id)
            .await
            .expect("cancel scheduled release");
        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list queue after cancel");
        assert_eq!(queue[0].status, "canceled");
        assert_eq!(queue[0].scheduled_publish_at, None);
        let releases = apps
            .list_app_releases()
            .await
            .expect("list release after cancel");
        let release = releases
            .iter()
            .find(|item| item.id == upload.release_id)
            .expect("canceled release");
        assert_eq!(release.status, "received");
        assert_eq!(release.scheduled_publish_at, None);
    }

    #[tokio::test]
    async fn release_package_upload_validates_version_code_and_allows_explicit_code() {
        let (apps, _db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "orders-version-code").await;

        let missing_code_err = apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "build-main".to_owned(),
                version_code: None,
                published_at: "2026-06-23T10:00:00Z".to_owned(),
                file_name: "orders-version-code-build-main.bin".to_owned(),
                bytes: b"build-main".to_vec(),
                entry_file: String::new(),
                source: "openapi".to_owned(),
            })
            .await
            .expect_err("non-semver release without version code should fail");
        assert!(matches!(missing_code_err, AppError::InvalidInput(_)));
        assert!(missing_code_err.message().contains("versionCode"));

        let zero_code_err = apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.2.3".to_owned(),
                version_code: Some(0),
                published_at: "2026-06-23T10:00:00Z".to_owned(),
                file_name: "orders-version-code_v1_2_3.bin".to_owned(),
                bytes: b"v1.2.3".to_vec(),
                entry_file: String::new(),
                source: "openapi".to_owned(),
            })
            .await
            .expect_err("zero version code should fail");
        assert!(matches!(zero_code_err, AppError::InvalidInput(_)));
        assert!(zero_code_err.message().contains("大于 0"));

        let uploaded = apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "build-main".to_owned(),
                version_code: Some(20260705),
                published_at: String::new(),
                file_name: "orders-version-code-build-main.bin".to_owned(),
                bytes: b"build-main".to_vec(),
                entry_file: String::new(),
                source: "openapi".to_owned(),
            })
            .await
            .expect("explicit version code should allow non-semver package version");

        assert_eq!(uploaded.version_code, 20260705);
        assert_eq!(uploaded.queue_id, None);
        assert!(!uploaded.published_at.trim().is_empty());
        let release = apps
            .list_app_releases()
            .await
            .expect("list releases")
            .into_iter()
            .find(|item| item.id == uploaded.release_id)
            .expect("uploaded release");
        assert_eq!(release.status, "received");
        assert_eq!(release.version, "build-main");
    }

    #[tokio::test]
    async fn queued_release_can_be_canceled_and_invalid_queue_states_are_rejected() {
        let (apps, _db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "orders-cancel-queue").await;
        let upload = upload_manual_release(&apps, app_id, "v1.2.3").await;
        apps.schedule_release_publish(upload.release_id, "2030-01-02T03:04:05Z")
            .await
            .expect("schedule release");
        let queue_id = apps
            .list_app_release_queue()
            .await
            .expect("list scheduled queue")
            .into_iter()
            .find(|item| item.release_id == upload.release_id)
            .expect("scheduled queue")
            .id;

        let canceled_app_id = apps
            .cancel_release_queue_item(queue_id)
            .await
            .expect("cancel scheduled queue through generic queue endpoint");

        assert_eq!(canceled_app_id, app_id);
        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list canceled queue");
        let canceled = queue
            .iter()
            .find(|item| item.id == queue_id)
            .expect("canceled queue item");
        assert_eq!(canceled.status, "canceled");
        assert_eq!(canceled.scheduled_publish_at, None);
        let release = apps
            .list_app_releases()
            .await
            .expect("list release after cancel")
            .into_iter()
            .find(|item| item.id == upload.release_id)
            .expect("canceled release");
        assert_eq!(release.status, "received");

        let canceled_again = apps
            .cancel_release_queue_item(queue_id)
            .await
            .expect_err("canceled queue should not be cancelable again");
        assert!(matches!(canceled_again, AppError::InvalidInput(_)));
        assert!(canceled_again.message().contains("只能取消等待中"));

        let missing = apps
            .cancel_release_queue_item(9_999_999)
            .await
            .expect_err("missing queue should fail");
        assert!(matches!(missing, AppError::InvalidInput(_)));
        assert!(missing.message().contains("发布队列项不存在"));
    }

    #[tokio::test]
    async fn manual_publish_falls_back_to_latest_snapshot_for_legacy_release_metadata() {
        let (apps, db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "orders-legacy-snapshot").await;
        let upload = upload_manual_release(&apps, app_id, "v1.2.3").await;

        apps.update_app_config(UpdateAppConfigInput {
            app_id,
            compose_content: "services:\n  app:\n    image: nginx:stable\n".to_owned(),
            env_content: "RUST_LOG=debug".to_owned(),
            deploy_scripts: DeployScriptSet {
                pre_deploy: "echo latest pre".to_owned(),
                deploy: String::new(),
                post_deploy: "echo latest post".to_owned(),
                switch_traffic: String::new(),
                cleanup: String::new(),
            },
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
            health_check: Default::default(),
        })
        .await
        .expect("update app config after upload");
        let latest_snapshot = apps
            .app_detail(app_id)
            .await
            .expect("app detail")
            .config_snapshots
            .into_iter()
            .next()
            .expect("latest snapshot");
        assert_ne!(latest_snapshot.id, upload.config_snapshot_id);

        sqlx::query("UPDATE app_releases SET metadata = '{}' WHERE id = ?1")
            .bind(upload.release_id)
            .execute(&db)
            .await
            .expect("simulate legacy release metadata without snapshot id");

        let queue_id = apps
            .publish_release_now(upload.release_id, "admin")
            .await
            .expect("publish legacy release")
            .expect("queue id");
        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list release queue")
            .into_iter()
            .find(|item| item.id == queue_id)
            .expect("legacy release queue item");

        assert_eq!(queue.config_snapshot_id, Some(latest_snapshot.id));
    }

    #[tokio::test]
    async fn due_scheduled_releases_are_moved_to_queue_order() {
        let (apps, db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "billing-api").await;
        let first = upload_manual_release(&apps, app_id, "v1.0.0").await;
        let second = upload_manual_release(&apps, app_id, "v1.1.0").await;
        apps.schedule_release_publish(first.release_id, "2030-01-02T03:04:05Z")
            .await
            .expect("schedule first release");
        apps.schedule_release_publish(second.release_id, "2030-01-02T03:05:05Z")
            .await
            .expect("schedule second release");
        sqlx::query(
            r#"
            UPDATE app_release_queue
            SET scheduled_publish_at = CASE release_id
                WHEN ?1 THEN '2000-01-01T00:00:00Z'
                WHEN ?2 THEN '2000-01-01T00:01:00Z'
            END
            WHERE release_id IN (?1, ?2)
            "#,
        )
        .bind(first.release_id)
        .bind(second.release_id)
        .execute(&db)
        .await
        .expect("move scheduled releases into the past");

        let due_app_ids = enqueue_due_scheduled_releases(&db)
            .await
            .expect("enqueue due releases");

        assert_eq!(due_app_ids, [app_id, app_id]);
        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list due release queue");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].release_id, first.release_id);
        assert_eq!(queue[1].release_id, second.release_id);
        assert!(queue.iter().all(|item| item.status == "queued"));
        assert!(queue.iter().all(|item| item.scheduled_publish_at.is_none()));
    }

    #[tokio::test]
    async fn manual_publish_uses_release_snapshot_not_latest_config() {
        let (apps, _db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "orders-snapshot").await;
        let upload = upload_manual_release(&apps, app_id, "v1.2.3").await;

        apps.update_app_config(UpdateAppConfigInput {
            app_id,
            compose_content: "services:\n  app:\n    image: nginx:stable\n".to_owned(),
            env_content: "RUST_LOG=debug".to_owned(),
            deploy_scripts: DeployScriptSet {
                pre_deploy: "echo changed pre".to_owned(),
                deploy: "docker compose up -d".to_owned(),
                post_deploy: String::new(),
                switch_traffic: String::new(),
                cleanup: String::new(),
            },
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
            health_check: Default::default(),
        })
        .await
        .expect("update app config after upload");

        let detail = apps.app_detail(app_id).await.expect("app detail");
        let latest_snapshot = detail
            .config_snapshots
            .first()
            .expect("latest config snapshot");
        assert_ne!(latest_snapshot.id, upload.config_snapshot_id);
        assert!(latest_snapshot.revision_no > upload.config_revision_no);

        let queue_id = apps
            .publish_release_now(upload.release_id, "admin")
            .await
            .expect("publish release now")
            .expect("queue id");

        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list release queue");
        let queued = queue
            .iter()
            .find(|item| item.id == queue_id)
            .expect("published queue item");
        assert_eq!(queued.release_id, upload.release_id);
        assert_eq!(queued.config_snapshot_id, Some(upload.config_snapshot_id));
        assert_ne!(queued.config_snapshot_id, Some(latest_snapshot.id));
    }

    #[tokio::test]
    async fn scheduled_release_rejects_duplicate_active_plan() {
        let (apps, _db, _data_dir) = app_service().await;
        let app_id = create_manual_compose_app(&apps, "orders-duplicate-schedule").await;
        let upload = upload_manual_release(&apps, app_id, "v1.2.3").await;

        apps.schedule_release_publish(upload.release_id, "2030-01-02T03:04:05Z")
            .await
            .expect("schedule release");

        let err = apps
            .schedule_release_publish(upload.release_id, "2030-01-02T04:04:05Z")
            .await
            .expect_err("duplicate schedule should fail");

        assert!(matches!(err, AppError::Conflict(_)));
        assert!(err.message().contains("已经在发布队列中"));
        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list release queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0].scheduled_publish_at.as_deref(),
            Some("2030-01-02T03:04:05Z")
        );
    }

    #[tokio::test]
    async fn auto_queue_release_preserves_receive_order_not_version_code_order() {
        let (apps, _db, _data_dir) = app_service().await;
        let app_id = apps
            .create_app(CreateAppInput {
                app_key: "orders-auto-order".to_owned(),
                name: "orders auto order".to_owned(),
                description: String::new(),
                environment: "test".to_owned(),
                app_type: "compose".to_owned(),
                deploy_strategy: "rolling_stop_on_failure".to_owned(),
                release_source: "package_upload".to_owned(),
                auto_queue_release: true,
                work_dir: "/opt/easy-deploy/apps/orders-auto-order".to_owned(),
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
            .expect("create auto queue app");
        let newer = apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v2.0.0".to_owned(),
                version_code: Some(2_000_000),
                published_at: "2026-06-23T10:00:00Z".to_owned(),
                file_name: "orders-auto-order-v2.bin".to_owned(),
                bytes: b"newer release".to_vec(),
                entry_file: String::new(),
                source: "web".to_owned(),
            })
            .await
            .expect("upload newer release");
        let older_late = apps
            .upload_release_package(UploadReleasePackageInput {
                app_id,
                release_version: "v1.9.0".to_owned(),
                version_code: Some(1_009_000),
                published_at: "2026-06-23T10:01:00Z".to_owned(),
                file_name: "orders-auto-order-v1-9.bin".to_owned(),
                bytes: b"older late release".to_vec(),
                entry_file: String::new(),
                source: "web".to_owned(),
            })
            .await
            .expect("upload older late release");

        assert!(newer.queued);
        assert!(older_late.queued);
        let queue = apps
            .list_app_release_queue()
            .await
            .expect("list release queue");
        let app_queue = queue
            .iter()
            .filter(|item| item.app_id == app_id)
            .map(|item| (item.release_id, item.version.as_str(), item.queue_seq))
            .collect::<Vec<_>>();

        assert_eq!(app_queue.len(), 2);
        assert_eq!(app_queue[0].0, newer.release_id);
        assert_eq!(app_queue[0].1, "v2.0.0");
        assert_eq!(app_queue[1].0, older_late.release_id);
        assert_eq!(app_queue[1].1, "v1.9.0");
        assert!(app_queue[0].2 < app_queue[1].2);
    }

    #[tokio::test]
    async fn auto_queued_releases_run_serially_and_record_task_steps() {
        let (apps, db, data_dir) = app_service().await;
        let app_id = create_auto_compose_app(&apps, "orders-serial").await;
        let first = upload_manual_release(&apps, app_id, "v1.0.0").await;
        let second = upload_manual_release(&apps, app_id, "v1.0.1").await;

        let first_queue = wait_for_release_queue_status(&apps, first.release_id, "success").await;
        let second_queue = wait_for_release_queue_status(&apps, second.release_id, "success").await;

        assert!(first_queue.queue_seq < second_queue.queue_seq);
        let first_task_id = first_queue.task_id.expect("first task id");
        let second_task_id = second_queue.task_id.expect("second task id");
        assert!(first_task_id < second_task_id);
        let releases = apps.list_app_releases().await.expect("list releases");
        let statuses = releases
            .iter()
            .filter(|release| release.app_id == app_id)
            .map(|release| (release.version.as_str(), release.status.as_str()))
            .collect::<Vec<_>>();
        assert!(statuses.contains(&("v1.0.0", "deployed")));
        assert!(statuses.contains(&("v1.0.1", "deployed")));

        let task_rows = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT status, phase
            FROM operation_tasks
            WHERE id IN (?1, ?2)
            ORDER BY id ASC
            "#,
        )
        .bind(first_task_id)
        .bind(second_task_id)
        .fetch_all(&db)
        .await
        .expect("read release tasks");
        assert_eq!(
            task_rows,
            vec![
                ("success".to_owned(), "completed".to_owned()),
                ("success".to_owned(), "completed".to_owned())
            ]
        );

        let step_keys = sqlx::query_scalar::<_, String>(
            r#"
            SELECT step_key
            FROM operation_task_steps
            WHERE task_id = ?1
            ORDER BY id ASC
            "#,
        )
        .bind(first_task_id)
        .fetch_all(&db)
        .await
        .expect("read task steps");
        assert!(step_keys.contains(&"node.preflight".to_owned()));
        assert!(step_keys.contains(&"compose.config".to_owned()));
        assert!(step_keys.contains(&"compose.action".to_owned()));
        assert!(step_keys.contains(&"script.pre_deploy".to_owned()));
        assert!(step_keys.contains(&"script.post_deploy".to_owned()));
        assert!(step_keys.contains(&"script.switch_traffic".to_owned()));
        assert!(step_keys.contains(&"script.cleanup".to_owned()));

        let combined_logs = sqlx::query_scalar::<_, String>(
            r#"
            SELECT COALESCE(GROUP_CONCAT(content, CHAR(10)), '')
            FROM operation_task_logs
            WHERE task_id IN (?1, ?2)
            "#,
        )
        .bind(first_task_id)
        .bind(second_task_id)
        .fetch_one(&db)
        .await
        .expect("read task logs");
        assert!(combined_logs.contains("版本 v1.0.0 已进入串行发布队列"));
        assert!(combined_logs.contains("版本 v1.0.1 已进入串行发布队列"));
        assert!(combined_logs.contains("已更新当前生效版本指针"));
        assert!(data_dir.path().join("apps/orders-serial/current").is_file());
    }

    #[tokio::test]
    async fn auto_queued_release_runs_remote_compose_sync_and_updates_ssh_runtime_state() {
        let runner = Arc::new(RecordingCommandRunner::default());
        let (apps, db, data_dir) = app_service_with_runner(runner.clone()).await;
        let ssh_node_id = create_ssh_target_node(&db, &data_dir).await;
        let app_id = create_auto_compose_app(&apps, "orders-ssh").await;
        sqlx::query("DELETE FROM app_targets WHERE app_id = ?1")
            .bind(app_id)
            .execute(&db)
            .await
            .expect("clear local target");
        sqlx::query(
            "INSERT INTO app_targets(app_id, node_id, target_role) VALUES (?1, ?2, 'primary')",
        )
        .bind(app_id)
        .bind(ssh_node_id)
        .execute(&db)
        .await
        .expect("bind ssh target");
        sqlx::query(
            r#"
            INSERT INTO app_runtime_states(app_id, node_id, runtime_status, message)
            VALUES (?1, ?2, 'unknown', '等待首次部署')
            "#,
        )
        .bind(app_id)
        .bind(ssh_node_id)
        .execute(&db)
        .await
        .expect("insert ssh runtime state");

        let release = upload_manual_release(&apps, app_id, "v2.0.0").await;
        let queue = wait_for_release_queue_status(&apps, release.release_id, "success").await;

        assert_eq!(queue.version, "v2.0.0");
        let runtime = sqlx::query_as::<_, (String, String, i64, Option<i64>)>(
            r#"
            SELECT runtime_status, active_version, service_count, last_task_id
            FROM app_runtime_states
            WHERE app_id = ?1 AND node_id = ?2
            "#,
        )
        .bind(app_id)
        .bind(ssh_node_id)
        .fetch_one(&db)
        .await
        .expect("read ssh runtime state");
        assert_eq!(runtime.0, "healthy");
        assert_eq!(runtime.1, "v2.0.0");
        assert_eq!(runtime.2, 1);
        assert_eq!(runtime.3, queue.task_id);

        let specs = runner.specs.lock().expect("lock command specs").clone();
        assert!(specs.iter().any(|spec| spec.program == "scp"));
        assert!(specs.iter().any(|spec| {
            spec.program == "ssh"
                && spec.args.contains(&"deploy@10.0.2.11".to_owned())
                && spec
                    .args
                    .iter()
                    .any(|arg| arg.contains("/opt/easy-deploy/apps/orders-ssh"))
        }));
        assert!(specs.iter().any(|spec| {
            spec.program == "ssh"
                && spec.args.windows(3).any(|window| {
                    window[0] == "docker" && window[1] == "compose" && window[2] == "up"
                })
        }));
    }

    #[tokio::test]
    async fn failed_auto_queue_blocks_later_release_and_marks_remaining_nodes_skipped() {
        let runner = Arc::new(ComposeUpFailureRunner::default());
        let (apps, db, data_dir) = app_service_with_runner(runner.clone()).await;
        let ssh_node_id = create_ssh_target_node(&db, &data_dir).await;
        let app_id = create_auto_compose_app(&apps, "orders-fail-block").await;
        sqlx::query(
            "INSERT INTO app_targets(app_id, node_id, target_role) VALUES (?1, ?2, 'primary')",
        )
        .bind(app_id)
        .bind(ssh_node_id)
        .execute(&db)
        .await
        .expect("bind second ssh target");
        sqlx::query(
            r#"
            INSERT INTO app_runtime_states(app_id, node_id, runtime_status, message)
            VALUES (?1, ?2, 'unknown', '等待首次部署')
            "#,
        )
        .bind(app_id)
        .bind(ssh_node_id)
        .execute(&db)
        .await
        .expect("insert ssh runtime state");
        let first = upload_manual_release(&apps, app_id, "v3.0.0").await;
        let second = upload_manual_release(&apps, app_id, "v3.0.1").await;

        let failed_queue = wait_for_release_queue_status(&apps, first.release_id, "failed").await;
        for _ in 0..30 {
            let queue = apps.list_app_release_queue().await.expect("list queue");
            let later = queue
                .iter()
                .find(|item| item.release_id == second.release_id)
                .expect("second release queue item");
            assert_eq!(
                later.status, "queued",
                "later release should stay queued after first release failed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let task_id = failed_queue.task_id.expect("failed task id");
        let task = apps.tasks.task_detail(task_id).await.expect("task detail");
        assert_eq!(task.status, "failed");
        assert!(task.summary.contains("compose up failed"));
        let node_results = sqlx::query_as::<_, (String, String)>(
            r#"
            SELECT node_key, status
            FROM operation_task_node_results
            WHERE task_id = ?1
            ORDER BY id ASC
            "#,
        )
        .bind(task_id)
        .fetch_all(&db)
        .await
        .expect("read task node results");
        assert_eq!(
            node_results,
            vec![
                ("local".to_owned(), "failed".to_owned()),
                ("ssh-prod-a".to_owned(), "skipped".to_owned())
            ]
        );

        let releases = apps.list_app_releases().await.expect("list releases");
        let first_release = releases
            .iter()
            .find(|release| release.id == first.release_id)
            .expect("first release");
        let second_release = releases
            .iter()
            .find(|release| release.id == second.release_id)
            .expect("second release");
        assert_eq!(first_release.status, "failed");
        assert_eq!(second_release.status, "queued");
    }

    #[test]
    fn normalize_compose_content_strips_top_level_version() {
        let content = normalize_compose_content(
            "version: '3.8'\nservices:\n  web:\n    image: nginx\n",
            "web",
        )
        .expect("normalize compose");

        assert!(!content.contains("version: '3.8'"));
        assert!(content.starts_with("services:\n"));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn normalize_compose_content_keeps_nested_version_key() {
        let content = normalize_compose_content(
            "services:\n  web:\n    image: nginx\n    labels:\n      version: stable\n",
            "web",
        )
        .expect("normalize compose");

        assert!(content.contains("      version: stable"));
    }

    #[test]
    fn normalize_compose_content_enforces_local_bind_mount_convention() {
        let content = normalize_compose_content(
            r#"
services:
  redis:
    image: redis:7-alpine
    volumes:
      - ./data/redis:/data
  alloy:
    image: grafana/alloy:v1.6.1
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock:ro
"#,
            "infra",
        )
        .expect("local bind mounts should be accepted");

        assert!(content.contains("./data/redis:/data"));

        let absolute = normalize_compose_content(
            r#"
services:
  redis:
    image: redis:7-alpine
    volumes:
      - /data/redis:/data
"#,
            "infra",
        )
        .expect_err("absolute data path should fail");
        assert!(absolute.message().contains("compose.yaml 同级目录"));

        let named = normalize_compose_content(
            r#"
services:
  redis:
    image: redis:7-alpine
    volumes:
      - redis_data:/data
"#,
            "infra",
        )
        .expect_err("named volume should fail");
        assert!(named.message().contains("目录约定"));

        let parent = normalize_compose_content(
            r#"
services:
  redis:
    image: redis:7-alpine
    volumes:
      - ./../redis:/data
"#,
            "infra",
        )
        .expect_err("parent traversal should fail");
        assert!(parent.message().contains("应用目录内"));

        let dot_segment = normalize_compose_content(
            r#"
services:
  redis:
    image: redis:7-alpine
    volumes:
      - ././redis:/data
"#,
            "infra",
        )
        .expect_err("dot segment should fail");
        assert!(dot_segment.message().contains("应用目录内"));
    }

    #[test]
    fn normalize_deploy_work_dir_keeps_directory_level_contract() {
        assert_eq!(
            normalize_deploy_work_dir(r" \opt\easy-deploy\apps\orders-api\ ")
                .expect("normalized deploy dir"),
            "/opt/easy-deploy/apps/orders-api"
        );
        assert!(
            normalize_deploy_work_dir("/opt/easy-deploy/apps/orders-api/compose.yaml").is_err()
        );
        assert!(
            normalize_deploy_work_dir("/opt/easy-deploy/apps/orders-api/docker-compose.yml")
                .is_err()
        );
        assert!(normalize_deploy_work_dir("relative/orders-api").is_err());
    }

    #[test]
    fn friendly_command_error_removes_common_prefixes_and_limits_lines() {
        let message = friendly_command_error(
            "time=\"2026-06-01\" level=warning msg=noise\nError response from daemon: Cannot connect\nERROR: invalid compose\nignored line\n",
            "fallback",
        );

        assert_eq!(message, "Cannot connect；invalid compose；ignored line");
    }

    #[test]
    fn friendly_command_error_uses_fallback_for_empty_output() {
        assert_eq!(
            friendly_command_error("\n  \n", "docker compose config 返回非 0 状态"),
            "docker compose config 返回非 0 状态"
        );
    }

    #[test]
    fn parse_compose_host_ports_reads_short_and_long_syntax() {
        let ports = parse_compose_host_ports(
            r#"
services:
  web:
    image: nginx
    ports:
      - "127.0.0.1:8080:80"
      - "8443:443/tcp"
      - "80"
  api:
    image: nginx
    ports:
      - target: 9000
        published: "19000"
        protocol: tcp
"#,
        )
        .expect("parse ports");

        assert_eq!(ports, [8080, 8443, 19000]);
    }

    #[test]
    fn parse_compose_host_ports_ignores_dynamic_host_ports() {
        let ports = parse_compose_host_ports(
            r#"
services:
  web:
    image: nginx
    ports:
      - "80"
      - target: 9000
"#,
        )
        .expect("parse ports");

        assert!(ports.is_empty());
    }

    #[test]
    fn remote_copy_files_preserve_release_and_script_layout() {
        let root = tempdir().expect("create temp dir");
        let app_dir = root.path();
        let release_dir = app_dir.join(RELEASES_DIR_NAME).join("v1.2.3");
        let scripts_dir = app_dir.join(META_DIR_NAME).join("scripts");
        fs::create_dir_all(release_dir.join("bundle/bin")).expect("create release dirs");
        fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        fs::write(
            release_dir.join(RELEASE_META_FILE_NAME),
            "release_version: v1.2.3\n",
        )
        .expect("write release metadata");
        fs::write(release_dir.join("bundle/bin/server"), "binary").expect("write bundle file");
        fs::write(scripts_dir.join("20-migrate.sh"), "#!/usr/bin/env sh\n").expect("write script");
        fs::write(
            app_dir.join(CURRENT_RELEASE_FILE_NAME),
            "release_version: v1.2.3\n",
        )
        .expect("write current");

        let mut files =
            collect_remote_copy_files(&release_dir, "/opt/easy-deploy/apps/orders/releases/v1.2.3")
                .expect("collect release files");
        files.extend(
            collect_remote_copy_files(
                &scripts_dir,
                "/opt/easy-deploy/apps/orders/.easy-deploy/scripts",
            )
            .expect("collect scripts"),
        );
        files.push(RemoteCopyFile {
            local_path: app_dir.join(CURRENT_RELEASE_FILE_NAME),
            remote_path: remote_join("/opt/easy-deploy/apps/orders", CURRENT_RELEASE_FILE_NAME),
        });
        let remote_paths = files
            .iter()
            .map(|file| file.remote_path.as_str())
            .collect::<Vec<_>>();

        assert!(
            remote_paths.contains(&"/opt/easy-deploy/apps/orders/releases/v1.2.3/release.yaml")
        );
        assert!(
            remote_paths
                .contains(&"/opt/easy-deploy/apps/orders/releases/v1.2.3/bundle/bin/server")
        );
        assert!(
            remote_paths
                .contains(&"/opt/easy-deploy/apps/orders/.easy-deploy/scripts/20-migrate.sh")
        );
        assert!(remote_paths.contains(&"/opt/easy-deploy/apps/orders/current"));
    }

    #[test]
    fn select_uploaded_binary_release_prunes_keeps_recent_uploads_and_active_release() {
        let rows = [
            uploaded_binary_artifact(6, "v1.6.0"),
            uploaded_binary_artifact(5, "v1.5.0"),
            uploaded_binary_artifact(4, "v1.4.0"),
            uploaded_binary_artifact(3, "v1.3.0"),
            uploaded_binary_artifact(2, "v1.2.0"),
            uploaded_binary_artifact(1, "v1.1.0"),
            manual_binary_artifact(7, "manual-v1"),
        ];

        let (ids, versions) = select_uploaded_binary_release_prunes(&rows, "v1.1.0", 4);

        assert_eq!(ids, [2]);
        assert_eq!(versions, ["v1.2.0"]);
    }

    #[test]
    fn select_uploaded_binary_release_prunes_treats_zero_as_one() {
        let rows = [
            uploaded_binary_artifact(3, "v1.3.0"),
            uploaded_binary_artifact(2, "v1.2.0"),
            uploaded_binary_artifact(1, "v1.1.0"),
        ];

        let (ids, versions) = select_uploaded_binary_release_prunes(&rows, "v1.3.0", 0);

        assert_eq!(ids, [2, 1]);
        assert_eq!(versions, ["v1.2.0", "v1.1.0"]);
    }

    #[test]
    fn parse_binary_package_name_normalizes_service_and_version() {
        let parsed =
            parse_binary_package_name("orders-api-prod_version_1_2_3.tar.gz").expect("parse");

        assert_eq!(parsed.service_key, "orders-api-prod");
        assert_eq!(parsed.release_version, "v1.2.3");
        assert_eq!(parsed.version_code, 1_002_003);

        let jar = parse_binary_package_name("orders-api-prod_version_v1.2.3.jar").expect("parse");
        assert_eq!(jar.service_key, "orders-api-prod");
        assert_eq!(jar.release_version, "v1.2.3");
        assert_eq!(jar.version_code, 1_002_003);
    }

    #[test]
    fn version_code_from_release_uses_semver_ordering() {
        assert_eq!(version_code_from_release("v1.2.3"), Some(1_002_003));
        assert_eq!(version_code_from_release("2_10_4"), Some(2_010_004));
        assert_eq!(version_code_from_release("bad"), None);
    }

    #[test]
    fn parse_binary_package_name_rejects_invalid_name() {
        let err = parse_binary_package_name("orders-api-prod-v1.2.3.tar.gz")
            .expect_err("invalid package name");

        assert_eq!(err.code(), "INVALID_PACKAGE_VERSION_NAME");
    }

    #[test]
    fn parse_binary_package_name_for_service_rejects_mismatch() {
        let err = parse_binary_package_name_for_service(
            "orders-api_version_1_2_3.tar.gz",
            "payments",
            None,
        )
        .expect_err("service mismatch");

        assert_eq!(err.code(), "PACKAGE_SERVICE_KEY_MISMATCH");
        assert!(err.message().contains("payments"));
        assert!(err.message().contains("orders-api"));
    }

    #[test]
    fn parse_binary_package_name_for_service_rejects_explicit_version_conflict() {
        let err = parse_binary_package_name_for_service(
            "orders-api_version_1_2_3.tar.gz",
            "orders-api",
            Some("v1.2.4"),
        )
        .expect_err("version conflict");

        assert_eq!(err.code(), "PACKAGE_VERSION_CONFLICT");
        assert!(err.message().contains("v1.2.4"));
        assert!(err.message().contains("v1.2.3"));
    }

    #[test]
    fn runtime_snapshot_metadata_records_binary_runtime_config() {
        let config = binary_config_item("v1.2.3", "/opt/app/releases/v1.2.3/app");

        let metadata = runtime_snapshot_metadata(
            "manual",
            "/data/apps/app",
            Some("v1.2.3"),
            Some(&DeployScriptSet::default()),
            Some(&config),
        );
        let value = serde_json::from_str::<JsonValue>(&metadata).expect("metadata json");
        let binary = value.get("binary").expect("binary metadata");

        assert_eq!(value["source"], "manual");
        assert_eq!(value["version"], "v1.2.3");
        assert_eq!(binary["artifact_version"], "v1.2.3");
        assert_eq!(binary["artifact_path"], "/opt/app/releases/v1.2.3/app");
        assert_eq!(binary["exec_args"], "--port 8080");
        assert_eq!(binary["unit_name"], "easy-deploy-worker-bin.service");
        assert_eq!(binary["release_strategy"], "blue_green");
        assert_eq!(binary["proxy_kind"], "caddy");
    }

    #[test]
    fn binary_config_from_snapshot_restores_runtime_config_and_artifact() {
        let app = app_detail_item("worker-bin", "/opt/app");
        let current = binary_config_item("v1.0.0", "/opt/app/releases/v1.0.0/app");
        let mut snapshot_config = binary_config_item("v1.2.3", "/opt/app/releases/v1.2.3/app");
        snapshot_config.exec_args = "--worker-count 4".to_owned();
        snapshot_config.base_port = 9000;
        snapshot_config.standby_port = 19000;
        snapshot_config.proxy_domain = "worker.example.com".to_owned();
        let snapshot = app_config_snapshot_item(
            "v1.2.3",
            &runtime_snapshot_metadata(
                "manual",
                "/data/apps/app",
                Some("v1.2.3"),
                Some(&DeployScriptSet::default()),
                Some(&snapshot_config),
            ),
            "RUST_LOG=debug",
        );
        let artifact = binary_artifact(9, "v1.2.3", r#"{"source":"upload"}"#);

        let restored = binary_config_from_snapshot(&app, &snapshot, &current, Some(&artifact));

        assert_eq!(restored.artifact_version, "v1.2.3");
        assert_eq!(restored.artifact_path, "/tmp/v1.2.3");
        assert_eq!(restored.exec_args, "--worker-count 4");
        assert_eq!(restored.base_port, 9000);
        assert_eq!(restored.standby_port, 19000);
        assert_eq!(restored.proxy_domain, "worker.example.com");
        assert_eq!(restored.env_content, "RUST_LOG=debug\n");
    }

    #[test]
    fn parse_compose_services_reads_image_ports_and_replicas() {
        let services = parse_compose_services(
            r#"
services:
  worker:
    image: busybox
  web:
    image: nginx:alpine
    ports:
      - "8080:80"
      - target: 9000
        published: "19000"
    deploy:
      replicas: 2
"#,
        )
        .expect("parse services");

        assert_eq!(
            services,
            [
                ParsedService {
                    name: "web".to_owned(),
                    image: "nginx:alpine".to_owned(),
                    ports: "8080:80, 19000:9000".to_owned(),
                    replicas: "2".to_owned(),
                },
                ParsedService {
                    name: "worker".to_owned(),
                    image: "busybox".to_owned(),
                    ports: "未声明端口".to_owned(),
                    replicas: "1".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn occupied_ports_detects_bound_tcp_port() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind dynamic port");
        let port = listener.local_addr().expect("local addr").port();

        assert_eq!(occupied_ports(&[port]), [port]);
    }

    #[test]
    fn binary_blue_green_job_targets_standby_slot() {
        let mut job = binary_task_job();

        assert_eq!(job.target_slot(), "green");
        assert_eq!(
            job.execution_unit_name(),
            "easy-deploy-worker-bin-green.service"
        );
        assert_eq!(job.target_port(), 18080);
        assert_eq!(job.promoted_slot(true).as_deref(), Some("green"));

        job.active_slot = "green".to_owned();
        assert_eq!(job.target_slot(), "blue");
        assert_eq!(
            job.execution_unit_name(),
            "easy-deploy-worker-bin-blue.service"
        );
        assert_eq!(job.target_port(), 8080);
    }

    #[test]
    fn binary_blue_green_health_endpoint_uses_target_port() {
        let mut job = binary_task_job();

        assert_eq!(
            job.slot_health_endpoint("http://127.0.0.1:8080/healthz"),
            "http://127.0.0.1:18080/healthz"
        );
        assert_eq!(
            job.slot_health_endpoint("127.0.0.1:8080"),
            "127.0.0.1:18080"
        );

        job.active_slot = "green".to_owned();
        assert_eq!(
            job.slot_health_endpoint("http://127.0.0.1:18080/healthz"),
            "http://127.0.0.1:8080/healthz"
        );
    }

    #[test]
    fn binary_restart_strategy_keeps_primary_unit_and_endpoint() {
        let mut job = binary_task_job();
        job.release_strategy = "restart".to_owned();

        assert_eq!(job.execution_unit_name(), "easy-deploy-worker-bin.service");
        assert_eq!(
            job.slot_health_endpoint("http://127.0.0.1:8080/healthz"),
            "http://127.0.0.1:8080/healthz"
        );
        assert_eq!(job.promoted_slot(true), None);
    }

    #[test]
    fn binary_target_artifact_path_only_matches_release_file_under_deploy_dir() {
        assert_eq!(
            binary_target_artifact_path(
                "/opt/easy-deploy/apps/worker-bin",
                "/opt/easy-deploy/apps/worker-bin/releases/v1.1.0/worker-bin",
            )
            .as_deref(),
            Some("/opt/easy-deploy/apps/worker-bin/releases/v1.1.0/worker-bin")
        );
        assert_eq!(
            binary_target_artifact_path(
                "/opt/easy-deploy/apps/worker-bin",
                "/opt/easy-deploy/artifacts/worker-bin",
            ),
            None
        );
    }

    #[test]
    fn service_runtime_overview_prioritizes_deploying_and_latest_message() {
        let empty = service_runtime_overview(&[]);
        assert_eq!(empty.status, "unknown");
        assert!(empty.active_version.is_empty());
        assert!(!empty.summary.trim().is_empty());

        let states = vec![
            runtime_state(
                1,
                "healthy",
                "v1.2.0",
                "ready",
                Some("2026-06-05T00:00:00Z"),
                "2026-06-01T00:00:00Z",
            ),
            runtime_state(
                2,
                "deploying",
                "v1.2.0",
                "switching",
                None,
                "2026-06-02T00:00:00Z",
            ),
            runtime_state(3, "unknown", "", "", None, "2026-06-03T00:00:00Z"),
        ];

        let overview = service_runtime_overview(&states);

        assert_eq!(overview.status, "deploying");
        assert_eq!(overview.active_version, "v1.2.0");
        assert_eq!(overview.latest_message, "ready");
        assert_eq!(overview.latest_checked_at, "2026-06-05T00:00:00Z");
        assert!(overview.summary.contains('1'));
    }

    #[test]
    fn service_target_node_item_uses_runtime_state_or_defaults() {
        let node = target_node(7, "node-a");

        let default_item = service_target_node_item(&node, &[]);
        assert_eq!(default_item.id, 7);
        assert_eq!(default_item.node_key, "node-a");
        assert_eq!(default_item.runtime_status, "unknown");
        assert!(default_item.active_version.is_empty());
        assert_eq!(default_item.service_count, 0);
        assert_eq!(default_item.last_task_id, None);

        let states = [runtime_state(
            7,
            "healthy",
            "v1.4.0",
            "all good",
            Some("2026-06-05T00:00:00Z"),
            "2026-06-04T00:00:00Z",
        )];
        let item = service_target_node_item(&node, &states);

        assert_eq!(item.runtime_status, "healthy");
        assert_eq!(item.active_version, "v1.4.0");
        assert_eq!(item.service_count, 2);
        assert_eq!(item.message, "all good");
        assert_eq!(item.last_task_id, Some(700));
        assert_eq!(item.last_task_status.as_deref(), Some("completed"));
        assert_eq!(item.last_task_kind.as_deref(), Some("compose.up"));
        assert_eq!(item.last_deploy_at.as_deref(), Some("2026-06-05T00:00:00Z"));
        assert_eq!(item.updated_at, "2026-06-04T00:00:00Z");
    }

    #[test]
    fn select_service_log_node_uses_first_or_requested_node() {
        let nodes = [target_node(1, "node-a"), target_node(2, "node-b")];

        assert_eq!(
            select_service_log_node(&nodes, None, "empty")
                .expect("first node")
                .id,
            1
        );
        assert_eq!(
            select_service_log_node(&nodes, Some(2), "empty")
                .expect("requested node")
                .node_key,
            "node-b"
        );
        assert!(select_service_log_node(&nodes, Some(3), "empty").is_err());
        assert!(select_service_log_node(&[], None, "empty").is_err());
    }

    #[test]
    fn deploy_diff_reports_no_baseline_unchanged_and_binary_changes() {
        let mut app = app_detail_item("worker-bin", "/opt/app");
        let baseline_binary = binary_config_item("v1.0.0", "/opt/app/releases/v1.0.0/app");
        let baseline = app_config_snapshot_item(
            "v1.0.0",
            &runtime_snapshot_metadata(
                "manual",
                "/opt/app",
                Some("v1.0.0"),
                Some(&DeployScriptSet::default()),
                Some(&baseline_binary),
            ),
            "RUST_LOG=info\n",
        );

        let no_baseline = build_deploy_diff(
            &app,
            "services:\n  worker:\n    image: nginx\n",
            "RUST_LOG=info\n",
            &baseline_binary,
            None,
        );
        assert_eq!(no_baseline.status, AppDeployDiffStatus::NoBaseline);
        assert!(no_baseline.rows.is_empty());

        let unchanged = build_deploy_diff(
            &app,
            &baseline.compose_content,
            &baseline.env_content,
            &baseline_binary,
            Some(&baseline),
        );
        assert_eq!(unchanged.status, AppDeployDiffStatus::Unchanged);
        assert!(unchanged.rows.iter().all(|row| !row.changed));

        let current_binary = binary_config_item("v1.2.0", "/opt/app/releases/v1.2.0/app");
        let changed = build_deploy_diff(
            &app,
            "services:\n  worker:\n    image: redis\n",
            "RUST_LOG=debug\n",
            &current_binary,
            Some(&baseline),
        );

        assert_eq!(changed.baseline_snapshot_id, Some(1));
        assert_eq!(changed.status, AppDeployDiffStatus::Changed);
        assert!(changed.rows.len() > 2);
        assert!(
            changed
                .rows
                .iter()
                .any(|row| row.label == "Compose" && row.changed)
        );
        assert!(
            changed
                .rows
                .iter()
                .any(|row| row.current_summary.contains("v1.2.0"))
        );

        app.app_type = "compose".to_owned();
        let compose_only = build_deploy_diff(
            &app,
            "services:\n  worker:\n    image: redis\n",
            "RUST_LOG=debug\n",
            &current_binary,
            Some(&baseline),
        );
        assert_eq!(compose_only.rows.len(), 2);
    }

    #[test]
    fn normalize_binary_proxy_config_defaults_and_validates_inputs() {
        let disabled = normalize_binary_proxy_config(
            false,
            "nginx",
            "not checked",
            "relative/path",
            "restart",
            "app",
        )
        .expect("disabled proxy");
        assert_eq!(
            disabled,
            (
                0,
                "nginx".to_owned(),
                "not checked".to_owned(),
                "relative/path".to_owned()
            )
        );

        let enabled = normalize_binary_proxy_config(
            true,
            "caddy",
            "api.example.com",
            "",
            "blue_green",
            "orders-api",
        )
        .expect("enabled caddy proxy");
        assert_eq!(enabled.0, 1);
        assert_eq!(enabled.1, "caddy");
        assert_eq!(enabled.2, "api.example.com");
        assert_eq!(enabled.3, "/etc/caddy/Caddyfile.d/orders-api.caddy");

        let windows_path = normalize_binary_proxy_config(
            true,
            "nginx",
            "127.0.0.1",
            "C:/nginx/conf.d/app.conf",
            "blue_green",
            "orders-api",
        )
        .expect("windows absolute path");
        assert_eq!(windows_path.3, "C:/nginx/conf.d/app.conf");

        assert!(
            normalize_binary_proxy_config(true, "caddy", "api.example.com", "", "restart", "app")
                .is_err()
        );
        assert!(
            normalize_binary_proxy_config(
                true,
                "haproxy",
                "api.example.com",
                "",
                "blue_green",
                "app"
            )
            .is_err()
        );
        assert!(
            normalize_binary_proxy_config(
                true,
                "caddy",
                "-bad.example.com",
                "",
                "blue_green",
                "app"
            )
            .is_err()
        );
        assert!(
            normalize_binary_proxy_config(
                true,
                "caddy",
                "api.example.com",
                "../Caddyfile",
                "blue_green",
                "app",
            )
            .is_err()
        );
    }

    #[test]
    fn normalize_binary_config_applies_defaults_and_rejects_bad_user() {
        let config = normalize_binary_config(NormalizeBinaryConfigInput {
            app_key: "orders-api",
            work_dir: "/opt/apps/orders-api",
            artifact_version: " v1.2.3 ",
            artifact_path: " /tmp/orders-api ",
            exec_args: " --port 8080 ",
            service_user: "",
            unit_name: "",
            release_strategy: "",
            active_slot: "",
            base_port: 0,
            standby_port: 18080,
            proxy_enabled: false,
            proxy_kind: "",
            proxy_domain: "",
            proxy_config_path: "",
            env_content: "RUST_LOG=info",
        })
        .expect("normalize binary config");

        assert_eq!(config.service_name, "orders-api");
        assert_eq!(config.service_user, "deploy");
        assert_eq!(config.unit_name, "easy-deploy-orders-api.service");
        assert_eq!(config.release_strategy, "restart");
        assert_eq!(config.active_slot, "blue");
        assert_eq!(config.base_port, 0);
        assert_eq!(config.proxy_enabled, 0);
        assert_eq!(config.proxy_kind, "none");
        assert_eq!(config.env_content, "RUST_LOG=info\n");

        assert!(
            normalize_binary_config(NormalizeBinaryConfigInput {
                app_key: "orders-api",
                work_dir: "/opt/apps/orders-api",
                artifact_version: "",
                artifact_path: "",
                exec_args: "",
                service_user: "bad user",
                unit_name: "",
                release_strategy: "",
                active_slot: "",
                base_port: 0,
                standby_port: 0,
                proxy_enabled: false,
                proxy_kind: "",
                proxy_domain: "",
                proxy_config_path: "",
                env_content: "",
            })
            .is_err()
        );
    }

    #[test]
    fn artifact_metadata_and_entry_helpers_handle_boundaries() {
        let metadata = render_artifact_metadata(ArtifactMetadataInput {
            source: "openapi",
            source_detail: "ai-agent",
            unit_name: "orders-api.service",
            uploaded_path: "/var/lib/easy-deploy/artifacts/orders-api",
            original_file_name: "orders-api_version_1_2_3.tar.gz",
            entry_file: "bin/server",
            sha256: "abc123",
            size_bytes: 123,
            config_snapshot_id: Some(42),
            config_revision_no: Some(7),
        });

        assert_eq!(artifact_metadata_value(&metadata, "source"), "openapi");
        assert_eq!(artifact_metadata_value(&metadata, "size_bytes"), "123");
        assert_eq!(release_metadata_snapshot_id(&metadata), Some(42));
        let with_snapshot = release_metadata_with_snapshot(r#"{"source":"upload"}"#, 9, Some(3))
            .expect("attach snapshot");
        assert_eq!(artifact_metadata_value(&with_snapshot, "source"), "upload");
        assert_eq!(
            artifact_metadata_value(&with_snapshot, "config_snapshot_id"),
            "9"
        );
        assert_eq!(
            artifact_metadata_value(&with_snapshot, "config_revision_no"),
            "3"
        );
        let repaired_array =
            release_metadata_with_snapshot("[]", 10, None).expect("repair non-object metadata");
        assert_eq!(
            artifact_metadata_value(&repaired_array, "config_snapshot_id"),
            "10"
        );
        assert!(release_metadata_with_snapshot("{", 1, None).is_err());

        assert_eq!(upload_source("  "), "upload");
        assert_eq!(artifact_channel_from_source("web"), "web");
        assert_eq!(artifact_channel_from_source("ai-agent"), "openapi");
        assert_eq!(artifact_kind_from_file_name("bundle.tgz"), "tar_gz");
        assert_eq!(artifact_kind_from_file_name("server.jar"), "binary");
        assert_eq!(
            normalize_entry_file("", "server.jar", "binary").expect("default binary entry"),
            "server.jar"
        );
        assert_eq!(
            normalize_entry_file("bin/server", "bundle.tgz", "tar_gz").expect("explicit tar entry"),
            "bin/server"
        );
        assert!(normalize_entry_file("", "bundle.tgz", "tar_gz").is_err());
        assert!(normalize_entry_file("../server", "bundle.tgz", "tar_gz").is_err());
        assert_eq!(
            sanitize_archive_path(Path::new("./bin/server")).expect("safe archive path"),
            PathBuf::from("bin").join("server")
        );
        assert!(sanitize_archive_path(Path::new("../server")).is_err());
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn command_output_helpers_order_failures_and_prepend_context() {
        let success = ComposeCommandOutput {
            command: "docker compose config".to_owned(),
            success: true,
            status_code: Some(0),
            output: "valid".to_owned(),
        };
        let failed = ComposeCommandOutput {
            command: "docker compose up".to_owned(),
            success: false,
            status_code: Some(17),
            output: "boom".to_owned(),
        };

        let merged = merge_command_outputs(
            vec![success.clone(), failed.clone()],
            false,
            "fallback command",
        );

        assert_eq!(merged.command, "docker compose up");
        assert_eq!(merged.status_code, Some(17));
        assert!(merged.output.starts_with("$ docker compose up\nboom"));
        assert!(merged.output.contains("$ docker compose config\nvalid"));

        let contextual = prepend_failure_context(merged, "preflight failed");
        assert!(contextual.output.starts_with("preflight failed\n"));
        let unchanged = prepend_failure_context(success, "ignored");
        assert_eq!(unchanged.output, "valid");
        let fallback = merge_command_outputs(Vec::new(), true, "fallback command");
        assert_eq!(fallback.command, "fallback command");
        assert_eq!(fallback.status_code, Some(0));
    }

    #[test]
    fn compose_runtime_helpers_count_paths_and_dedupe_values() {
        assert_eq!(
            count_compose_running_lines(
                "NAME IMAGE STATUS\napi nginx running\ntime=\"2026\" level=warning\n---\nworker busybox running\n"
            ),
            2
        );
        assert_eq!(
            target_work_dir_path(r"C:\apps\", "orders/current"),
            "C:/apps/orders/current"
        );
        assert_eq!(target_work_dir_path("", "orders/current"), "orders/current");
        assert_eq!(dedupe_ids(&[3, 1, 3, 2, 1]), vec![3, 1, 2]);
        assert_eq!(
            dedupe_strings(&[
                " api ".to_owned(),
                String::new(),
                "api".to_owned(),
                "worker".to_owned(),
            ]),
            vec!["api".to_owned(), "worker".to_owned()]
        );
    }

    #[test]
    fn top_level_normalizers_accept_defaults_and_reject_invalid_values() {
        assert_eq!(
            normalize_deploy_strategy("").expect("default deploy strategy"),
            "rolling_stop_on_failure"
        );
        assert_eq!(
            normalize_deploy_strategy("rolling_continue").expect("explicit deploy strategy"),
            "rolling_continue"
        );
        assert!(normalize_deploy_strategy("parallel").is_err());
        assert_eq!(
            parse_deploy_strategy("rolling_continue"),
            DeployStrategy::RollingContinue
        );
        assert_eq!(
            parse_deploy_strategy("bad"),
            DeployStrategy::RollingStopOnFailure
        );

        assert_eq!(
            normalize_release_source("").expect("default release source"),
            "package_upload"
        );
        assert_eq!(
            normalize_release_source("manual").expect("manual release source"),
            "manual"
        );
        assert!(normalize_release_source("git_tag").is_err());
        assert_eq!(release_status_after_upload(true), "queued");
        assert_eq!(release_status_after_upload(false), "received");

        assert_eq!(
            normalize_key(" Orders_API ").expect("normalize key"),
            "orders_api"
        );
        assert!(normalize_key("orders api").is_err());
        assert_eq!(
            normalize_app_type("compose").expect("compose type"),
            "compose"
        );
        assert!(normalize_app_type("systemd").is_err());
        assert_eq!(
            normalize_app_environment("").expect("default environment"),
            "test"
        );
        assert_eq!(
            normalize_app_environment("prod").expect("production alias"),
            "production"
        );
        assert!(normalize_app_environment("staging").is_err());
        assert_eq!(
            normalize_release_id("v1.2.3").expect("release id"),
            "v1.2.3"
        );
        assert!(normalize_release_id("../v1").is_err());
    }

    #[test]
    fn published_at_normalization_accepts_utc_and_local_times() {
        assert_eq!(normalize_published_at("").expect("empty time"), None);
        assert_eq!(
            normalize_published_at("2026-06-23T10:00:00+08:00")
                .expect("rfc3339 time")
                .as_deref(),
            Some("2026-06-23T02:00:00Z")
        );
        assert_eq!(
            normalize_published_at("2026-06-23T10:00")
                .expect("local datetime")
                .as_deref(),
            Some("2026-06-23T02:00:00Z")
        );
        assert_eq!(
            normalize_published_at("release-window-2026")
                .expect("safe custom value")
                .as_deref(),
            Some("release-window-2026")
        );
        assert!(normalize_published_at("bad value with spaces").is_err());
    }

    #[test]
    fn binary_path_and_proxy_helpers_render_expected_values() {
        let root = tempdir().expect("runtime root");
        let unit_path = binary_systemd_unit_path(root.path(), "worker.service");
        assert_eq!(
            unit_path,
            root.path()
                .join(META_DIR_NAME)
                .join(SYSTEMD_DIR_NAME)
                .join("worker.service")
        );
        assert_eq!(
            remote_binary_systemd_unit_path("/opt/apps/worker/", "worker.service")
                .expect("remote unit path"),
            "/opt/apps/worker/.easy-deploy/systemd/worker.service"
        );
        assert!(remote_binary_systemd_unit_path("relative", "worker.service").is_err());
        assert_eq!(
            binary_command_work_dir("relative", root.path()),
            root.path().to_path_buf()
        );
        assert_eq!(
            binary_command_work_dir(
                root.path().to_string_lossy().as_ref(),
                Path::new("fallback")
            ),
            root.path().to_path_buf()
        );

        let mut job = binary_task_job();
        job.proxy_kind = "caddy".to_owned();
        job.proxy_domain = "worker.example.com".to_owned();
        job.proxy_config_path = String::new();
        assert_eq!(
            binary_proxy_config_path(&job).expect("default proxy path"),
            "/etc/caddy/Caddyfile.d/worker-bin.caddy"
        );
        assert_eq!(
            proxy_config_file_name(&job).expect("caddy file name"),
            "worker-bin.caddy"
        );
        let caddy = render_binary_proxy_config(&job).expect("caddy config");
        assert!(caddy.contains("worker.example.com"));
        assert!(caddy.contains("127.0.0.1:18080"));

        job.proxy_kind = "nginx".to_owned();
        job.proxy_config_path = "/etc/nginx/conf.d/worker.conf".to_owned();
        assert_eq!(
            binary_proxy_config_path(&job).expect("custom proxy path"),
            "/etc/nginx/conf.d/worker.conf"
        );
        assert_eq!(
            proxy_config_file_name(&job).expect("nginx file name"),
            "worker-bin.conf"
        );
        let nginx = render_binary_proxy_config(&job).expect("nginx config");
        assert!(nginx.contains("server_name worker.example.com"));
        assert!(nginx.contains("proxy_pass http://127.0.0.1:18080"));

        assert_eq!(
            remote_parent_path("/etc/nginx/conf.d/worker.conf").unwrap(),
            "/etc/nginx/conf.d"
        );
        assert!(remote_parent_path("worker.conf").is_err());
        assert_eq!(proxy_systemd_service_name("nginx"), "nginx.service");
        assert_eq!(proxy_systemd_service_name("caddy"), "caddy.service");
        assert_eq!(binary_proxy_kind_label("nginx"), "Nginx");
        assert_eq!(binary_proxy_kind_label("caddy"), "Caddy");

        job.proxy_kind = "none".to_owned();
        assert!(render_binary_proxy_config(&job).is_err());
        assert!(proxy_config_file_name(&job).is_err());
    }

    #[test]
    fn remote_copy_and_node_work_dir_helpers_normalize_paths() {
        let root = tempdir().expect("copy root");
        let source = root.path().join("runtime");
        fs::create_dir_all(source.join("nested")).expect("create source dirs");
        fs::write(source.join("app.yaml"), "app").expect("write app file");
        fs::write(source.join("nested").join("release.yaml"), "release").expect("write release");

        let files =
            collect_remote_copy_files(&source, "/opt/apps/worker").expect("collect copy files");
        let remote_paths = files
            .iter()
            .map(|file| file.remote_path.as_str())
            .collect::<Vec<_>>();

        assert!(remote_paths.contains(&"/opt/apps/worker/app.yaml"));
        assert!(remote_paths.contains(&"/opt/apps/worker/nested/release.yaml"));
        assert_eq!(
            remote_parent_dirs(&files, "/opt/apps/worker"),
            vec![
                "/opt/apps/worker".to_owned(),
                "/opt/apps/worker/nested".to_owned()
            ]
        );
        assert!(collect_remote_copy_files(&source.join("missing"), "/opt/apps/worker").is_err());
        assert_eq!(
            normalize_remote_target_root(r"\opt\apps\worker\").expect("normalize remote root"),
            "/opt/apps/worker"
        );
        assert!(normalize_remote_target_root("/opt//apps").is_err());
        assert_eq!(remote_join("/opt/apps/", "/worker"), "/opt/apps/worker");

        let mut job = binary_task_job();
        let node = target_node(1, "node-a");
        job.deploy_work_dir = String::new();
        assert_eq!(
            binary_node_deploy_work_dir(&job, &node),
            "/opt/easy-deploy/apps/worker-bin"
        );
        job.deploy_work_dir = "/srv/worker".to_owned();
        assert_eq!(binary_node_deploy_work_dir(&job, &node), "/srv/worker");

        let mut app = app_detail_item("orders-api", "");
        assert_eq!(
            binary_node_deploy_work_dir_for_app(&app, &node),
            "/opt/easy-deploy/apps/orders-api"
        );
        app.work_dir = "/srv/orders".to_owned();
        assert_eq!(
            binary_node_deploy_work_dir_for_app(&app, &node),
            "/srv/orders"
        );
    }

    #[test]
    fn runtime_metadata_and_config_helpers_round_trip_binary_fields() {
        let mut app = app_detail_item("worker-bin", "/opt/app");
        app.description = r#"needs "quotes" and \slashes"#.to_owned();
        let config = binary_config_item("v2.0.0", "/opt/app/releases/v2.0.0/app");
        let metadata = render_runtime_metadata(
            &app,
            vec![TargetNodeMetadata {
                node_key: "node-a".to_owned(),
                name: r#"Node "A""#.to_owned(),
            }],
            "/var/lib/easy-deploy/apps/worker-bin",
            Some(&config),
        );

        assert!(metadata.contains(r#"app_key: "worker-bin""#));
        assert!(metadata.contains(r#"description: "needs \"quotes\" and \\slashes""#));
        assert!(metadata.contains(r#"node_key: "node-a""#));
        assert!(metadata.contains(r#"name: "Node \"A\"""#));
        assert!(metadata.contains(r#"artifact_version: "v2.0.0""#));
        assert!(
            metadata.contains(r#"env_file: ".easy-deploy/systemd/easy-deploy-worker-bin.env""#)
        );

        let runtime_metadata = to_binary_runtime_metadata(&config);
        assert_eq!(runtime_metadata.service_name, "worker-bin");
        assert!(runtime_metadata.proxy_enabled);
        let runtime_config = to_binary_runtime_config(9, "worker-bin", "Worker", &config);
        assert_eq!(runtime_config.app_id, 9);
        assert_eq!(runtime_config.name, "Worker");
        assert_eq!(runtime_config.env_content, "RUST_LOG=info\n");

        assert_eq!(binary_unit_env_file_name("worker.service"), "worker.env");
        assert_eq!(binary_unit_env_file_name("worker"), "worker.env");
        assert_eq!(
            binary_blue_green_unit_name("worker.service", "green"),
            "worker-green.service"
        );

        let restored = binary_config_from_metadata(&metadata);
        assert_eq!(restored.artifact_version, "v2.0.0");
        assert_eq!(restored.unit_name, "easy-deploy-worker-bin.service");
        assert_eq!(restored.base_port, 8080);
        assert_eq!(restored.proxy_enabled, 1);
        assert_eq!(binary_config_from_metadata("not yaml").unit_name, "");
    }

    #[test]
    fn deploy_script_snapshot_and_runtime_dir_helpers_load_scripts() {
        let scripts = deploy_scripts_from_snapshot_metadata(
            r#"{"deploy_scripts":{"pre_deploy":"pre","deploy":"deploy","post_deploy":"post","switch_traffic":"switch","cleanup":"clean"}}"#,
        );
        assert_eq!(scripts.pre_deploy, "pre");
        assert_eq!(scripts.deploy, "deploy");
        assert_eq!(scripts.post_deploy, "post");
        assert_eq!(scripts.switch_traffic, "switch");
        assert_eq!(scripts.cleanup, "clean");
        assert_eq!(
            deploy_scripts_from_snapshot_metadata("{}"),
            DeployScriptSet::default()
        );

        let root = tempdir().expect("script root");
        let scripts_dir = root.path().join(META_DIR_NAME).join("scripts");
        fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        fs::write(scripts_dir.join("pre_deploy.sh"), "pre").expect("write pre");
        fs::write(scripts_dir.join("deploy.sh"), "deploy").expect("write deploy");
        fs::write(scripts_dir.join("post_deploy.sh"), "post").expect("write post");
        fs::write(scripts_dir.join("switch_traffic.sh"), "switch").expect("write switch");
        fs::write(scripts_dir.join("cleanup.sh"), "clean").expect("write cleanup");

        let loaded = deploy_scripts_from_runtime_dir(root.path());
        assert_eq!(loaded.pre_deploy, "pre");
        assert_eq!(loaded.deploy, "deploy");
        assert_eq!(loaded.post_deploy, "post");
        assert_eq!(loaded.switch_traffic, "switch");
        assert_eq!(loaded.cleanup, "clean");
    }

    #[test]
    fn health_action_and_guard_helpers_cover_status_rules() {
        let mut app = app_detail_item("worker-bin", "/opt/app");
        assert!(ensure_app_enabled(&app).is_ok());
        app.status = "disabled".to_owned();
        assert!(ensure_app_enabled(&app).is_err());

        assert!(ensure_has_enabled_targets(&[target_node(1, "node-a")]).is_ok());
        assert!(ensure_has_enabled_targets(&[]).is_err());
        assert!(!task_phase_label("queued").trim().is_empty());
        assert!(!task_phase_label("missing").trim().is_empty());

        assert_eq!(
            ComposeTaskAction::from_task_kind("compose.restart"),
            Some(ComposeTaskAction::Restart)
        );
        assert_eq!(ComposeTaskAction::from_task_kind("bad"), None);
        assert_eq!(ComposeTaskAction::Up.task_kind(), "compose.up");
        assert_eq!(ComposeTaskAction::Down.deploy_action(), "compose_down");
        assert!(ComposeTaskAction::Restart.runs_health_check());
        assert!(!ComposeTaskAction::Down.runs_health_check());
        assert_eq!(
            ComposeTaskAction::Down.runtime_status(true, "completed"),
            "stopped"
        );
        assert_eq!(
            ComposeTaskAction::Up.runtime_status(false, "failed"),
            "unhealthy"
        );
        assert_eq!(
            compose_action_command_label(ComposeTaskAction::Restart),
            "docker compose restart"
        );

        assert_eq!(
            BinaryTaskAction::from_task_kind("binary.restart"),
            Some(BinaryTaskAction::Restart)
        );
        assert_eq!(BinaryTaskAction::from_task_kind("bad"), None);
        assert_eq!(BinaryTaskAction::Restart.task_kind(), "binary.restart");
        assert_eq!(BinaryTaskAction::Stop.deploy_action(), "binary_stop");
        assert!(BinaryTaskAction::Restart.runs_health_check());
        assert!(BinaryTaskAction::Restart.syncs_runtime_files());
        assert!(!BinaryTaskAction::Stop.syncs_runtime_files());
        assert_eq!(
            BinaryTaskAction::Stop.runtime_status(true, "completed"),
            "stopped"
        );
        assert_eq!(
            BinaryTaskAction::Restart.runtime_status(false, "failed"),
            "unhealthy"
        );
        assert_eq!(
            binary_action_command_label(BinaryTaskAction::Stop, "worker.service"),
            "systemctl stop worker.service"
        );

        let http = health_check_detail_text(
            &HealthCheckConfig {
                kind: HealthCheckKind::Http,
                endpoint: "http://127.0.0.1:8080/healthz".to_owned(),
                timeout_secs: 3,
                expected_status: 204,
            },
            None,
        );
        assert!(http.contains("http://127.0.0.1:8080/healthz"));
        assert!(http.contains("204"));

        let mut binary = binary_config_item("v1.0.0", "/opt/app");
        binary.active_slot = "green".to_owned();
        let systemd = health_check_detail_text(
            &HealthCheckConfig {
                kind: HealthCheckKind::SystemdActive,
                endpoint: "worker.service".to_owned(),
                timeout_secs: 5,
                expected_status: 200,
            },
            Some(&binary),
        );
        assert!(systemd.contains("easy-deploy-worker-bin-green.service"));
        assert_eq!(display_health_endpoint("", "fallback"), "fallback");
        assert_eq!(common_active_version(&[]), "");
        let mixed_version = common_active_version(&[
            runtime_state(1, "healthy", "v1.0.0", "", None, "2026-06-01T00:00:00Z"),
            runtime_state(2, "healthy", "v1.1.0", "", None, "2026-06-01T00:00:00Z"),
        ]);
        assert!(!mixed_version.trim().is_empty());
        assert_ne!(mixed_version, "v1.0.0");
        assert_ne!(mixed_version, "v1.1.0");
        assert!(!format_runtime_summary(0, 0, 0, 0, 0).trim().is_empty());
    }

    #[test]
    fn app_error_and_strategy_helpers_cover_display_and_labels() {
        let invalid = AppError::InvalidInput("bad input".to_owned());
        assert_eq!(invalid.message(), "bad input");
        assert_eq!(invalid.to_string(), "bad input");
        assert_eq!(
            AppError::from(RuntimeFsError::InvalidInput("runtime input".to_owned())).message(),
            "runtime input"
        );
        assert_eq!(
            AppError::from(RuntimeFsError::Io("runtime io".to_owned())).message(),
            "runtime io"
        );
        assert_eq!(
            AppError::from(DeployError::InvalidInput("deploy input".to_owned())).message(),
            "deploy input"
        );
        assert_eq!(
            AppError::from(DeployError::Command("deploy command".to_owned())).message(),
            "deploy command"
        );

        assert!(DeployStrategy::RollingStopOnFailure.should_stop_after_failure());
        assert!(!DeployStrategy::RollingContinue.should_stop_after_failure());
        assert_eq!(DeployStrategy::RollingContinue.as_str(), "rolling_continue");
        assert!(
            !DeployStrategy::RollingStopOnFailure
                .label()
                .trim()
                .is_empty()
        );
        assert!(!DeployStrategy::RollingContinue.label().trim().is_empty());
        assert_ne!(
            release_publish_mode_label(true),
            release_publish_mode_label(false)
        );
    }

    #[test]
    fn task_action_helpers_cover_remaining_variants() {
        assert_eq!(ComposeTaskAction::Down.task_kind(), "compose.down");
        assert_eq!(ComposeTaskAction::Restart.task_kind(), "compose.restart");
        assert_eq!(
            ComposeTaskAction::Restart.deploy_action(),
            "compose_restart"
        );
        assert!(!ComposeTaskAction::Down.title_prefix().trim().is_empty());
        assert!(!ComposeTaskAction::Restart.title_prefix().trim().is_empty());
        assert!(!ComposeTaskAction::Down.label().trim().is_empty());
        assert!(!ComposeTaskAction::Restart.label().trim().is_empty());
        assert_eq!(
            compose_action_command_label(ComposeTaskAction::Down),
            "docker compose down"
        );
        assert_eq!(
            ComposeTaskAction::Restart.runtime_status(true, "completed"),
            "healthy"
        );

        assert_eq!(BinaryTaskAction::Stop.task_kind(), "binary.stop");
        assert_eq!(BinaryTaskAction::Restart.deploy_action(), "binary_restart");
        assert!(!BinaryTaskAction::Restart.title_prefix().trim().is_empty());
        assert!(!BinaryTaskAction::Stop.title_prefix().trim().is_empty());
        assert!(!BinaryTaskAction::Restart.label().trim().is_empty());
        assert!(!BinaryTaskAction::Stop.label().trim().is_empty());
        assert!(!BinaryTaskAction::Stop.runs_health_check());
        assert_eq!(
            BinaryTaskAction::Restart.runtime_status(true, "completed"),
            "healthy"
        );
        assert_eq!(
            binary_action_command_label(BinaryTaskAction::Restart, "worker.service"),
            "systemctl restart worker.service"
        );
    }

    #[test]
    fn binary_normalizers_cover_path_domain_and_slot_edges() {
        assert!(normalize_key(" ").is_err());
        assert_eq!(
            default_proxy_config_path("nginx", "orders-api"),
            "/etc/nginx/conf.d/orders-api.conf"
        );
        assert_eq!(
            default_proxy_config_path("caddy", "orders-api"),
            "/etc/caddy/Caddyfile.d/orders-api.caddy"
        );
        assert!(normalize_proxy_config_path("relative.conf").is_err());
        assert!(normalize_proxy_config_path("/etc//bad.conf").is_err());
        assert!(normalize_proxy_config_path("/etc/../bad.conf").is_err());
        assert!(normalize_proxy_config_path("/etc/caddy/bad file.conf").is_err());
        assert_eq!(
            normalize_proxy_config_path("/etc/caddy/site.conf").expect("proxy path"),
            "/etc/caddy/site.conf"
        );

        assert!(is_valid_proxy_domain("localhost"));
        assert!(is_valid_proxy_domain("127.0.0.1"));
        assert!(is_valid_proxy_domain("api.example.com"));
        assert!(!is_valid_proxy_domain(".bad"));
        assert!(!is_valid_proxy_domain("bad."));
        assert!(!is_valid_proxy_domain("-bad.example.com"));
        assert!(!is_valid_proxy_domain("bad-.example.com"));
        assert!(!is_valid_proxy_domain("bad_label.example.com"));

        assert_eq!(
            normalize_binary_release_strategy("blue_green").expect("blue green"),
            "blue_green"
        );
        assert!(normalize_binary_release_strategy("rolling").is_err());
        assert_eq!(normalize_binary_slot("green").expect("green slot"), "green");
        assert!(normalize_binary_slot("red").is_err());
        assert_eq!(
            normalize_binary_port(65535, "port").expect("max port"),
            65535
        );
        assert!(normalize_binary_port(-1, "port").is_err());
        assert!(normalize_binary_port(65536, "port").is_err());
        assert_eq!(
            normalize_unit_name("worker@green.service", "fallback").expect("unit"),
            "worker@green.service"
        );
        assert!(normalize_unit_name("worker", "fallback").is_err());
        assert!(normalize_unit_name("bad worker.service", "fallback").is_err());

        let default_config = default_binary_config_for_app("orders-api", "/opt/orders-api");
        assert_eq!(default_config.service_name, "orders-api");
        assert_eq!(default_config.working_dir, "/opt/orders-api");
        assert_eq!(default_config.unit_name, "easy-deploy-orders-api.service");
        assert_eq!(
            required_text(" value ", "required").expect("required"),
            "value"
        );
        assert!(required_text(" ", "required").is_err());
    }

    #[test]
    fn release_package_helpers_cover_more_edge_cases() {
        let parsed = parse_release_package_name(r"C:\tmp\Orders_API_version_V1.2.3.zip")
            .expect("parse release package");
        assert_eq!(parsed.service_key, "orders_api");
        assert_eq!(parsed.release_version, "v1.2.3");
        assert_eq!(parsed.version_code, 1_002_003);

        let matched = parse_release_package_name_for_service(
            "orders-api_version_1_2_3.tgz",
            " Orders-API ",
            Some("1.2.3"),
        )
        .expect("matching service");
        assert_eq!(matched.release_version, "v1.2.3");
        assert_eq!(
            parse_release_package_name_for_service("bad_version_1_2_3.tgz", "orders-api", None)
                .expect_err("service mismatch")
                .code(),
            "PACKAGE_SERVICE_KEY_MISMATCH"
        );
        assert_eq!(
            parse_release_package_name_for_service(
                "orders-api_version_1_2_3.tgz",
                "orders-api",
                Some("1.2.4"),
            )
            .expect_err("version mismatch")
            .code(),
            "PACKAGE_VERSION_CONFLICT"
        );

        assert!(parse_release_package_name("bad name_version_1_2_3.tar.gz").is_err());
        assert!(parse_release_package_name("orders-api_version_1_2.tar.gz").is_err());
        assert!(parse_release_package_name("orders-api.tar.gz").is_err());
        assert_eq!(strip_binary_package_extension("bundle.zip"), "bundle");
        assert_eq!(strip_binary_package_extension("bundle.bin"), "bundle.bin");
        assert_eq!(
            normalize_package_version("01_002_0003").as_deref(),
            Some("v01.002.0003")
        );
        assert_eq!(normalize_package_version("1.2").as_deref(), None);
        assert_eq!(version_code_from_release("v0.0.1"), Some(1));
        assert_eq!(version_code_from_release("v1.2.3.4"), None);
        assert_eq!(
            parse_binary_package_name("worker_version_2_0_1.jar")
                .expect("binary package")
                .version_code,
            2_000_001
        );
    }

    #[test]
    fn metadata_snapshot_and_archive_helpers_cover_fallbacks() {
        let metadata = render_artifact_metadata(ArtifactMetadataInput {
            source: "upload",
            source_detail: "",
            unit_name: r#"worker\"unit.service"#,
            uploaded_path: "/tmp/worker",
            original_file_name: "worker_version_1_0_0.jar",
            entry_file: "worker.jar",
            sha256: "hash",
            size_bytes: 0,
            config_snapshot_id: None,
            config_revision_no: None,
        });
        assert_eq!(artifact_metadata_value(&metadata, "size_bytes"), "0");
        assert_eq!(
            artifact_metadata_value(r#"{"ok":true,"items":[1]}"#, "ok"),
            "true"
        );
        assert_eq!(artifact_metadata_value(r#"{"items":[1]}"#, "items"), "");
        assert_eq!(artifact_metadata_value("{", "source"), "");
        assert_eq!(
            release_metadata_snapshot_id(r#"{"config_snapshot_id":"9"}"#),
            None
        );
        let empty_snapshot =
            release_metadata_with_snapshot("", 7, None).expect("empty metadata snapshot");
        assert_eq!(
            artifact_metadata_value(&empty_snapshot, "config_snapshot_id"),
            "7"
        );

        let deploy_scripts = DeployScriptSet {
            pre_deploy: "pre".to_owned(),
            deploy: "deploy".to_owned(),
            post_deploy: String::new(),
            switch_traffic: String::new(),
            cleanup: String::new(),
        };
        let binary = binary_config_item("v1.0.0", "/opt/app/releases/v1.0.0/worker");
        let runtime_metadata = runtime_snapshot_metadata(
            "deploy",
            "/var/lib/easy-deploy/apps/worker",
            Some("v1.0.0"),
            Some(&deploy_scripts),
            Some(&binary),
        );
        let runtime_value =
            serde_json::from_str::<JsonValue>(&runtime_metadata).expect("runtime metadata json");
        assert_eq!(runtime_value["source"], "deploy");
        assert_eq!(runtime_value["version"], "v1.0.0");
        assert_eq!(runtime_value["deploy_scripts"]["pre_deploy"], "pre");
        assert_eq!(runtime_value["binary"]["artifact_version"], "v1.0.0");

        let app = app_detail_item("orders-api", "/srv/orders-api");
        let current = BinaryConfigItem::default();
        let snapshot = app_config_snapshot_item(
            "",
            r#"{"binary":{"artifact_version":"v2.0.0","artifact_path":"/pkg/orders","proxy_enabled":true}}"#,
            "KEY=value",
        );
        assert_eq!(snapshot_artifact_version(&snapshot), "v2.0.0");
        let restored = binary_config_from_snapshot(&app, &snapshot, &current, None);
        assert_eq!(restored.service_name, "orders-api");
        assert_eq!(restored.working_dir, "/srv/orders-api");
        assert_eq!(restored.service_user, "deploy");
        assert_eq!(restored.unit_name, "easy-deploy-orders-api.service");
        assert_eq!(restored.release_strategy, "restart");
        assert_eq!(restored.active_slot, "blue");
        assert_eq!(restored.artifact_version, "v2.0.0");
        assert_eq!(restored.artifact_path, "/pkg/orders");
        assert_eq!(restored.proxy_enabled, 1);
        assert_eq!(restored.env_content, "KEY=value\n");

        let direct_snapshot = app_config_snapshot_item(" v3.0.0 ", "{}", "");
        assert_eq!(snapshot_artifact_version(&direct_snapshot), "v3.0.0");
        let artifact = uploaded_binary_artifact(9, "v9.0.0");
        let artifact_restored =
            binary_config_from_snapshot(&app, &snapshot, &current, Some(&artifact));
        assert_eq!(artifact_restored.artifact_version, "v9.0.0");

        assert_eq!(
            sanitize_archive_path(Path::new("bin/server")).expect("archive path"),
            PathBuf::from("bin").join("server")
        );
        assert!(sanitize_archive_path(Path::new("/abs/server")).is_err());
        assert!(sanitize_archive_path(Path::new("")).is_err());
    }

    #[test]
    fn text_compose_and_cleanup_helpers_cover_edges() {
        let root = tempdir().expect("runtime root");
        assert_eq!(runtime_service_count(root.path()), 0);
        fs::write(
            root.path().join("compose.yaml"),
            "services:\n  api:\n    image: nginx\n  worker:\n    image: busybox\n",
        )
        .expect("write compose");
        assert_eq!(runtime_service_count(root.path()), 2);

        let release_root = root.path().join("releases");
        fs::create_dir_all(release_root.join("v0.1.0")).expect("create release");
        cleanup_pruned_binary_release_dirs(
            root.path(),
            &["v0.1.0".to_owned(), "missing".to_owned()],
        )
        .expect("cleanup pruned releases");
        assert!(!release_root.join("v0.1.0").exists());

        let compose = default_compose_content("orders-api");
        assert!(compose.contains("services:"));
        assert!(compose.contains("orders-api"));
        assert_eq!(normalize_env_content(" KEY=value "), "KEY=value\n");
        assert_eq!(normalize_env_content(""), "");
        assert_eq!(
            strip_top_level_compose_version("version: '3.8'\nservices:\n  api:\n    image: nginx"),
            "services:\n  api:\n    image: nginx"
        );
        assert!(is_compose_version_line("version: '3.8'"));
        assert!(!is_compose_version_line("version:"));
        assert!(!is_compose_version_line("services:"));
        assert_eq!(
            strip_common_error_prefix("Error response from daemon: denied"),
            "denied"
        );
        assert_eq!(
            strip_common_error_prefix("error during connect: refused"),
            "refused"
        );
        assert_eq!(ensure_trailing_newline("KEY=value"), "KEY=value\n");
        assert_eq!(ensure_trailing_newline("KEY=value\n"), "KEY=value\n");
        assert_eq!(ensure_trailing_newline(""), "");
        assert_eq!(json_escape(r#"a\b"c"#), r#"a\\b\"c"#);

        let fallback = friendly_command_error("time=\"2026\" level=warning\n", "fallback");
        assert_eq!(fallback, "fallback");
        let friendly = friendly_command_error("ERROR: boom\nError: nope\nplain", "fallback");
        assert!(friendly.contains("boom"));
        assert!(friendly.contains("nope"));
        assert!(friendly.contains("plain"));
        assert!(!friendly.contains("ERROR:"));
        assert_eq!(parse_port(" 8080 "), Some(8080));
        assert_eq!(parse_port("0"), None);
        assert_eq!(to_port(65535), Some(65535));
        assert_eq!(to_port(65536), None);
        assert_eq!(join_ports(&[443, 8080]), "443, 8080");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert!(!summarize_inline_value("").trim().is_empty());
        assert!(
            !summarize_config_content("services:\n  api:")
                .trim()
                .is_empty()
        );
        assert!(diff_preview(&"line\n".repeat(20)).ends_with("..."));
    }

    #[test]
    fn target_node_ssh_target_normalizes_identity_and_rejects_bad_port() {
        let mut node = target_node(1, "node-a");
        node.credential_private_key_path = Some(" C:/keys/id_rsa ".to_owned());
        let target = node.ssh_target().expect("ssh target");
        assert_eq!(target.address(), "127.0.0.1");
        assert_eq!(target.port(), 22);
        assert_eq!(
            target.identity_file().map(PathBuf::as_path),
            Some(Path::new("C:/keys/id_rsa"))
        );

        node.ssh_port = 0;
        assert!(node.ssh_target().is_err());
    }

    fn uploaded_binary_artifact(id: i64, version: &str) -> BinaryArtifactItem {
        binary_artifact(id, version, r#"{"source":"upload"}"#)
    }

    fn manual_binary_artifact(id: i64, version: &str) -> BinaryArtifactItem {
        binary_artifact(id, version, r#"{"source":"manual"}"#)
    }

    fn binary_artifact(id: i64, version: &str, metadata: &str) -> BinaryArtifactItem {
        BinaryArtifactItem {
            id,
            version: version.to_owned(),
            version_code: version_code_from_release(version).unwrap_or(id),
            artifact_path: format!("/tmp/{version}"),
            artifact_kind: "file".to_owned(),
            status: "registered".to_owned(),
            metadata: metadata.to_owned(),
            published_at: format!("2026-06-01T00:00:{id:02}Z"),
            created_at: format!("2026-06-01T00:00:{id:02}Z"),
        }
    }

    fn binary_config_item(version: &str, artifact_path: &str) -> BinaryConfigItem {
        BinaryConfigItem {
            service_name: "worker-bin".to_owned(),
            artifact_version: version.to_owned(),
            artifact_path: artifact_path.to_owned(),
            exec_args: "--port 8080".to_owned(),
            working_dir: "/opt/app".to_owned(),
            service_user: "deploy".to_owned(),
            unit_name: "easy-deploy-worker-bin.service".to_owned(),
            release_strategy: "blue_green".to_owned(),
            active_slot: "blue".to_owned(),
            base_port: 8080,
            standby_port: 18080,
            proxy_enabled: 1,
            proxy_kind: "caddy".to_owned(),
            proxy_domain: "worker.local".to_owned(),
            proxy_config_path: "/etc/caddy/Caddyfile.d/worker-bin.caddy".to_owned(),
            env_content: "RUST_LOG=info\n".to_owned(),
        }
    }

    fn app_detail_item(app_key: &str, work_dir: &str) -> AppDetailItem {
        AppDetailItem {
            id: 1,
            app_key: app_key.to_owned(),
            name: "Worker".to_owned(),
            description: String::new(),
            environment: "test".to_owned(),
            app_type: "binary".to_owned(),
            deploy_mode: "binary".to_owned(),
            deploy_strategy: "rolling_stop_on_failure".to_owned(),
            release_source: "package_upload".to_owned(),
            compose_strategy: "recreate".to_owned(),
            auto_queue_release: 1,
            work_dir: work_dir.to_owned(),
            status: "ready".to_owned(),
            target_names: None,
            target_count: 1,
            created_at: "2026-06-01T00:00:00Z".to_owned(),
            updated_at: "2026-06-01T00:00:00Z".to_owned(),
        }
    }

    fn app_config_snapshot_item(
        artifact_version: &str,
        metadata: &str,
        env_content: &str,
    ) -> AppConfigSnapshotItem {
        AppConfigSnapshotItem {
            id: 1,
            revision_no: 2,
            snapshot_kind: "manual".to_owned(),
            compose_content: String::new(),
            env_content: env_content.to_owned(),
            artifact_version: artifact_version.to_owned(),
            config_hash: "hash".to_owned(),
            metadata: metadata.to_owned(),
            created_at: "2026-06-01T00:00:00Z".to_owned(),
        }
    }

    fn binary_task_job() -> BinaryTaskJob {
        BinaryTaskJob {
            task_id: 1,
            app_id: 1,
            release_id: None,
            queue_id: None,
            app_key: "worker-bin".to_owned(),
            deploy_work_dir: "/opt/easy-deploy/apps/worker-bin".to_owned(),
            unit_name: "easy-deploy-worker-bin.service".to_owned(),
            artifact_version: "v1.0.0".to_owned(),
            artifact_path: "/opt/easy-deploy/apps/worker-bin/releases/v1.0.0/worker-bin".to_owned(),
            config_snapshot_id: Some(1),
            config_revision_no: 1,
            release_strategy: "blue_green".to_owned(),
            active_slot: "blue".to_owned(),
            base_port: 8080,
            standby_port: 18080,
            proxy_enabled: false,
            proxy_kind: "none".to_owned(),
            proxy_domain: String::new(),
            proxy_config_path: String::new(),
            deploy_strategy: DeployStrategy::RollingStopOnFailure,
            action: BinaryTaskAction::Restart,
        }
    }

    fn runtime_state(
        node_id: i64,
        runtime_status: &str,
        active_version: &str,
        message: &str,
        last_deploy_at: Option<&str>,
        updated_at: &str,
    ) -> AppRuntimeStateItem {
        AppRuntimeStateItem {
            node_id,
            node_name: format!("node-{node_id}"),
            node_key: format!("node-{node_id}"),
            runtime_status: runtime_status.to_owned(),
            active_version: active_version.to_owned(),
            service_count: 2,
            message: message.to_owned(),
            last_task_id: Some(node_id * 100),
            last_task_status: Some("completed".to_owned()),
            last_task_kind: Some("compose.up".to_owned()),
            last_deploy_at: last_deploy_at.map(str::to_owned),
            updated_at: updated_at.to_owned(),
        }
    }

    fn target_node(id: i64, node_key: &str) -> AppTargetNode {
        AppTargetNode {
            id,
            node_key: node_key.to_owned(),
            name: format!("Node {id}"),
            node_type: "local".to_owned(),
            status: "active".to_owned(),
            address: "127.0.0.1".to_owned(),
            ssh_port: 22,
            ssh_user: "root".to_owned(),
            credential_private_key_path: None,
            work_dir: "/opt/easy-deploy/apps".to_owned(),
            caddy_available: 1,
            nginx_available: 0,
        }
    }
}
