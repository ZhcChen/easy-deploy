use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::Mutex;

use crate::artifact_storage::{
    AliyunOssObjectVerifier, ArtifactObjectVerifier, ArtifactStorageConfig,
    STORAGE_PROVIDER_ALIYUN_OSS, STORAGE_PROVIDER_LOCAL,
};

pub const DEFAULT_STEP_LOG_HEAD_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_STEP_LOG_TAIL_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_TASK_LOG_BYTES: usize = 100 * 1024 * 1024;
pub const TASK_LOG_PREVIEW_HEAD_BYTES: usize = 8 * 1024;
pub const TASK_LOG_PREVIEW_TAIL_BYTES: usize = 24 * 1024;
pub const TASK_LOG_PREVIEW_MAX_STEPS: usize = 32;

#[derive(Clone)]
pub struct DeploymentLogService {
    db: SqlitePool,
    state: Arc<Mutex<DeploymentLogState>>,
    head_limit: usize,
    tail_limit: usize,
    task_limit: usize,
}

#[derive(Default)]
struct DeploymentLogState {
    tasks: HashMap<i64, Arc<Mutex<ActiveTaskLog>>>,
}

struct ActiveTaskLog {
    budget: TaskLogBudget,
    steps: HashMap<i64, ActiveStepLog>,
}

struct ActiveStepLog {
    buffer: BoundedLogBuffer,
    finished: bool,
    updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedTaskLogPreview {
    pub step_id: i64,
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
    pub stored_bytes: u64,
    pub dropped_bytes: u64,
    pub truncated: bool,
    pub preview_omitted_bytes: u64,
    pub updated_at: String,
    pub live: bool,
}

#[derive(sqlx::FromRow)]
struct BoundedTaskLogPreviewRow {
    step_id: i64,
    head: Vec<u8>,
    tail: Vec<u8>,
    stored_bytes: i64,
    dropped_bytes: i64,
    truncated: bool,
    updated_at: String,
}

struct PersistedStepLog {
    head: Vec<u8>,
    tail: Vec<u8>,
    received_bytes: u64,
    dropped_bytes: u64,
    head_limit: usize,
    tail_limit: usize,
    finished: bool,
    updated_at: String,
}

#[derive(sqlx::FromRow)]
struct PersistedTaskLogRow {
    stored_bytes: i64,
    received_bytes: i64,
    dropped_bytes: i64,
    max_bytes: i64,
}

#[derive(sqlx::FromRow)]
struct PersistedStepLogRow {
    step_updated_at: String,
    head: Option<Vec<u8>>,
    tail: Option<Vec<u8>>,
    received_bytes: Option<i64>,
    dropped_bytes: Option<i64>,
    head_limit: Option<i64>,
    tail_limit: Option<i64>,
    finished: Option<bool>,
    log_updated_at: Option<String>,
}

struct LogPersistenceSnapshot {
    step: BoundedLogSnapshot,
    head_limit: usize,
    tail_limit: usize,
    task_max_bytes: usize,
    task_stored_bytes: u64,
    task_received_bytes: u64,
    task_dropped_bytes: u64,
    task_truncated: bool,
    finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentRetentionError {
    NotFound(String),
    InvalidState(String),
    Database(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCleanupPreview {
    pub unit_release_id: i64,
    pub version: String,
    pub size_bytes: u64,
    pub artifact_status: String,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationReleaseCleanupPreview {
    pub app_release_id: i64,
    pub version: String,
    pub version_code: i64,
    pub immutable_status: String,
    pub estimated_bytes: u64,
    pub archive_blockers: Vec<String>,
    pub blockers: Vec<String>,
}

impl ApplicationReleaseCleanupPreview {
    pub fn can_archive(&self) -> bool {
        self.immutable_status == "ready" && self.archive_blockers.is_empty()
    }

    pub fn can_delete(&self) -> bool {
        self.immutable_status == "archived" && self.blockers.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentHistoryDeletePreview {
    pub deployment_run_id: i64,
    pub app_release_id: i64,
    pub task_id: Option<i64>,
    pub status: String,
    pub snapshot_status: String,
    pub log_reference_count: i64,
    pub unit_result_count: i64,
    pub queue_reference_count: i64,
    pub blockers: Vec<String>,
}

impl DeploymentHistoryDeletePreview {
    pub fn allowed(&self) -> bool {
        self.blockers.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentHistoryDeleteResult {
    pub deployment_run_id: i64,
    pub app_release_id: i64,
    pub task_id: Option<i64>,
    pub deleted_unit_results: u64,
    pub deleted_queue_rows: u64,
    pub cleared_task_release: bool,
}

impl ArtifactCleanupPreview {
    pub fn allowed(&self) -> bool {
        self.blockers.is_empty()
            && matches!(
                self.artifact_status.as_str(),
                "active" | "delete_failed" | "deleting"
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDeletionTarget {
    pub provider: String,
    pub package_path: String,
    pub extract_dir: String,
    pub bucket: String,
    pub endpoint: String,
    pub object_key: String,
    pub object_version_id: String,
}

#[async_trait]
pub trait ArtifactObjectDeleter: Send + Sync {
    async fn delete(&self, target: &ArtifactDeletionTarget) -> Result<(), String>;
}

#[derive(Clone)]
pub struct ArtifactStorageDeleter {
    data_dir: PathBuf,
    storage: ArtifactStorageConfig,
    oss: Arc<dyn ArtifactObjectVerifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCleanupResult {
    pub unit_release_id: i64,
    pub status: String,
    pub error: String,
}

#[derive(Clone)]
pub struct DeploymentRetentionService {
    db: SqlitePool,
}

impl ArtifactStorageDeleter {
    pub fn new(data_dir: impl AsRef<Path>, storage: ArtifactStorageConfig) -> Self {
        Self::with_oss_verifier(
            data_dir,
            storage,
            Arc::new(AliyunOssObjectVerifier::default()),
        )
    }

    pub fn with_oss_verifier(
        data_dir: impl AsRef<Path>,
        storage: ArtifactStorageConfig,
        oss: Arc<dyn ArtifactObjectVerifier>,
    ) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            storage,
            oss,
        }
    }
}

#[async_trait]
impl ArtifactObjectDeleter for ArtifactStorageDeleter {
    async fn delete(&self, target: &ArtifactDeletionTarget) -> Result<(), String> {
        match target.provider.as_str() {
            STORAGE_PROVIDER_LOCAL => {
                delete_local_artifact_paths(
                    &self.data_dir,
                    [&target.package_path, &target.extract_dir],
                )
                .await
            }
            STORAGE_PROVIDER_ALIYUN_OSS => {
                let version_id = target.object_version_id.trim();
                if version_id.is_empty() {
                    return Err("OSS 制品缺少精确对象版本号，拒绝删除当前版本".to_owned());
                }
                let mut config = self.storage.aliyun_oss.clone();
                config.bucket = target.bucket.trim().to_owned();
                if !target.endpoint.trim().is_empty() {
                    config.endpoint = target.endpoint.trim().to_owned();
                }
                self.oss
                    .delete(&config, &target.object_key, Some(version_id))
                    .await
                    .map_err(|error| error.to_string())
            }
            provider => Err(format!("不支持清理存储类型 {provider}")),
        }
    }
}

impl std::fmt::Display for DeploymentRetentionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) | Self::InvalidState(message) | Self::Database(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for DeploymentRetentionError {}

impl From<sqlx::Error> for DeploymentRetentionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl DeploymentLogService {
    pub fn new(db: SqlitePool) -> Self {
        Self::with_limits(
            db,
            DEFAULT_STEP_LOG_HEAD_BYTES,
            DEFAULT_STEP_LOG_TAIL_BYTES,
            DEFAULT_TASK_LOG_BYTES,
        )
    }

    pub fn with_limits(
        db: SqlitePool,
        head_limit: usize,
        tail_limit: usize,
        task_limit: usize,
    ) -> Self {
        Self {
            db,
            state: Arc::new(Mutex::new(DeploymentLogState::default())),
            head_limit,
            tail_limit,
            task_limit,
        }
    }

    pub async fn append(
        &self,
        task_id: i64,
        step_id: i64,
        secrets: &[String],
        chunk: &[u8],
    ) -> Result<BoundedLogSnapshot, DeploymentRetentionError> {
        self.append_buffered(task_id, step_id, secrets, chunk)
            .await?;
        self.checkpoint(task_id, step_id).await
    }

    pub async fn append_buffered(
        &self,
        task_id: i64,
        step_id: i64,
        secrets: &[String],
        chunk: &[u8],
    ) -> Result<(), DeploymentRetentionError> {
        self.mutate_buffer(task_id, step_id, secrets, Some(chunk), 0, false)
            .await
    }

    pub async fn record_dropped(
        &self,
        task_id: i64,
        step_id: i64,
        secrets: &[String],
        dropped_bytes: u64,
    ) -> Result<(), DeploymentRetentionError> {
        self.mutate_buffer(task_id, step_id, secrets, None, dropped_bytes, false)
            .await
    }

    pub async fn checkpoint(
        &self,
        task_id: i64,
        step_id: i64,
    ) -> Result<BoundedLogSnapshot, DeploymentRetentionError> {
        let Some(snapshot) = self.persistence_snapshot(task_id, step_id).await else {
            return self.snapshot(task_id, step_id).await;
        };
        self.persist(task_id, step_id, &snapshot).await?;
        Ok(snapshot.step)
    }

    pub async fn finish(
        &self,
        task_id: i64,
        step_id: i64,
    ) -> Result<BoundedLogSnapshot, DeploymentRetentionError> {
        self.mutate_buffer(task_id, step_id, &[], None, 0, true)
            .await?;
        let snapshot = self
            .persistence_snapshot(task_id, step_id)
            .await
            .ok_or_else(|| DeploymentRetentionError::NotFound("步骤日志不存在".to_owned()))?;
        self.persist(task_id, step_id, &snapshot).await?;
        if let Some(task) = self.active_task_if_present(task_id).await {
            task.lock().await.steps.remove(&step_id);
        }
        Ok(snapshot.step)
    }

    pub async fn snapshot(
        &self,
        task_id: i64,
        step_id: i64,
    ) -> Result<BoundedLogSnapshot, DeploymentRetentionError> {
        let row = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, i64, i64, i64, bool)>(
            r#"
            SELECT head_content, tail_content, stored_bytes, received_bytes,
                   dropped_bytes, truncated
            FROM deployment_step_log_buffers WHERE task_id = ?1 AND step_id = ?2
            "#,
        )
        .bind(task_id)
        .bind(step_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| DeploymentRetentionError::NotFound("步骤日志不存在".to_owned()))?;
        Ok(BoundedLogSnapshot {
            head: row.0,
            tail: row.1,
            stored_bytes: row.2 as u64,
            received_bytes: row.3 as u64,
            dropped_bytes: row.4 as u64,
            truncated: row.5,
        })
    }

    pub async fn task_previews(
        &self,
        task_id: i64,
    ) -> Result<Vec<BoundedTaskLogPreview>, DeploymentRetentionError> {
        self.task_previews_with_limits(
            task_id,
            TASK_LOG_PREVIEW_HEAD_BYTES,
            TASK_LOG_PREVIEW_TAIL_BYTES,
            TASK_LOG_PREVIEW_MAX_STEPS,
        )
        .await
    }

    async fn task_previews_with_limits(
        &self,
        task_id: i64,
        head_limit: usize,
        tail_limit: usize,
        max_steps: usize,
    ) -> Result<Vec<BoundedTaskLogPreview>, DeploymentRetentionError> {
        let rows = sqlx::query_as::<_, BoundedTaskLogPreviewRow>(
            r#"
            SELECT step_id,
                   substr(head_content, 1, ?2) AS head,
                   CASE WHEN ?3 = 0 THEN X'' ELSE substr(tail_content, -?3) END AS tail,
                   stored_bytes, dropped_bytes, truncated, updated_at
            FROM deployment_step_log_buffers
            WHERE task_id = ?1
            ORDER BY updated_at DESC, step_id DESC
            LIMIT ?4
            "#,
        )
        .bind(task_id)
        .bind(head_limit as i64)
        .bind(tail_limit as i64)
        .bind(max_steps.max(1) as i64)
        .fetch_all(&self.db)
        .await?;
        let mut previews = rows
            .into_iter()
            .map(|row| BoundedTaskLogPreview {
                step_id: row.step_id,
                preview_omitted_bytes: (row.stored_bytes
                    - row.head.len() as i64
                    - row.tail.len() as i64)
                    .max(0) as u64,
                head: row.head,
                tail: row.tail,
                stored_bytes: row.stored_bytes.max(0) as u64,
                dropped_bytes: row.dropped_bytes.max(0) as u64,
                truncated: row.truncated,
                updated_at: row.updated_at,
                live: false,
            })
            .collect::<Vec<_>>();

        if let Some(task) = self.active_task_if_present(task_id).await {
            let task = task.lock().await;
            for (step_id, step) in &task.steps {
                let mut preview = step.buffer.preview(head_limit, tail_limit);
                preview.step_id = *step_id;
                preview.updated_at = step.updated_at.clone();
                preview.live = true;
                if let Some(existing) = previews
                    .iter_mut()
                    .find(|existing| existing.step_id == *step_id)
                {
                    *existing = preview;
                } else {
                    previews.push(preview);
                }
            }
        }
        previews.sort_by(|left, right| {
            right
                .live
                .cmp(&left.live)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| right.step_id.cmp(&left.step_id))
        });
        previews.truncate(max_steps.max(1));
        previews.sort_by_key(|preview| preview.step_id);
        Ok(previews)
    }

    pub async fn delete_task_logs(
        &self,
        task_id: i64,
        operator: &str,
    ) -> Result<u64, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let task_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM operation_tasks WHERE id = ?1")
                .bind(task_id)
                .fetch_optional(&mut *tx)
                .await?;
        let task_status = task_status
            .ok_or_else(|| DeploymentRetentionError::NotFound("任务不存在".to_owned()))?;
        let reconciling: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM environment_deployment_runs
                WHERE task_id = ?1 AND status = 'reconciling'
            )
            "#,
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
        if matches!(task_status.as_str(), "queued" | "running") || reconciling {
            return Err(DeploymentRetentionError::InvalidState(
                "活动部署或待核对部署的日志不能清理".to_owned(),
            ));
        }
        let bounded_bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(stored_bytes), 0) FROM deployment_step_log_buffers WHERE task_id = ?1",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
        let legacy_bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(length(CAST(content AS BLOB))), 0) FROM operation_task_logs WHERE task_id = ?1",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM deployment_step_log_buffers WHERE task_id = ?1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM deployment_task_log_budgets WHERE task_id = ?1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM operation_task_logs WHERE task_id = ?1")
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        let released_bytes = (bounded_bytes + legacy_bytes).max(0) as u64;
        insert_cleanup_event(
            &mut tx,
            "task",
            task_id,
            "info",
            "部署任务日志已清理",
            operator,
            &format!("释放 {released_bytes} 字节，任务、阶段、步骤和单元结果继续保留"),
        )
        .await?;
        tx.commit().await?;
        self.state.lock().await.tasks.remove(&task_id);
        Ok(released_bytes)
    }

    async fn mutate_buffer(
        &self,
        task_id: i64,
        step_id: i64,
        secrets: &[String],
        chunk: Option<&[u8]>,
        dropped_bytes: u64,
        finish: bool,
    ) -> Result<(), DeploymentRetentionError> {
        let task = self.active_task(task_id).await?;
        let needs_step = !task.lock().await.steps.contains_key(&step_id);
        if needs_step {
            let persisted = self.load_step(task_id, step_id).await?;
            let step = ActiveStepLog {
                buffer: BoundedLogBuffer::from_persisted(
                    persisted.head_limit,
                    persisted.tail_limit,
                    persisted.head,
                    persisted.tail,
                    persisted.received_bytes,
                    persisted.dropped_bytes,
                    secrets.to_vec(),
                ),
                finished: persisted.finished,
                updated_at: persisted.updated_at,
            };
            task.lock().await.steps.entry(step_id).or_insert(step);
        }
        let mut task = task.lock().await;
        let ActiveTaskLog { budget, steps } = &mut *task;
        let step = steps
            .get_mut(&step_id)
            .expect("step was initialized before log mutation");
        if step.finished && !finish {
            return Err(DeploymentRetentionError::InvalidState(
                "步骤日志已经结束或不属于该任务".to_owned(),
            ));
        }
        if let Some(chunk) = chunk {
            step.buffer.append(chunk, budget);
        }
        if dropped_bytes > 0 {
            step.buffer.record_external_drop(dropped_bytes, budget);
        }
        if finish && !step.finished {
            step.buffer.finish(budget);
            step.finished = true;
        }
        Ok(())
    }

    async fn active_task(
        &self,
        task_id: i64,
    ) -> Result<Arc<Mutex<ActiveTaskLog>>, DeploymentRetentionError> {
        if let Some(task) = self.active_task_if_present(task_id).await {
            return Ok(task);
        }
        let persisted = sqlx::query_as::<_, PersistedTaskLogRow>(
            r#"
            SELECT stored_bytes, received_bytes, dropped_bytes, max_bytes
            FROM deployment_task_log_budgets WHERE task_id = ?1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.db)
        .await?;
        let budget = match persisted {
            Some(row) => TaskLogBudget::from_usage(
                row.max_bytes.max(1) as usize,
                row.stored_bytes.max(0) as usize,
                row.received_bytes.max(0) as u64,
                row.dropped_bytes.max(0) as u64,
            ),
            None => TaskLogBudget::new(self.task_limit),
        };
        let candidate = Arc::new(Mutex::new(ActiveTaskLog {
            budget,
            steps: HashMap::new(),
        }));
        let mut state = self.state.lock().await;
        Ok(state.tasks.entry(task_id).or_insert(candidate).clone())
    }

    async fn active_task_if_present(&self, task_id: i64) -> Option<Arc<Mutex<ActiveTaskLog>>> {
        self.state.lock().await.tasks.get(&task_id).cloned()
    }

    async fn load_step(
        &self,
        task_id: i64,
        step_id: i64,
    ) -> Result<PersistedStepLog, DeploymentRetentionError> {
        let row = sqlx::query_as::<_, PersistedStepLogRow>(
            r#"
            SELECT steps.updated_at AS step_updated_at,
                   logs.head_content AS head, logs.tail_content AS tail,
                   logs.received_bytes, logs.dropped_bytes,
                   logs.head_limit_bytes AS head_limit,
                   logs.tail_limit_bytes AS tail_limit,
                   logs.finished, logs.updated_at AS log_updated_at
            FROM operation_task_steps steps
            LEFT JOIN deployment_step_log_buffers logs ON logs.step_id = steps.id
            WHERE steps.id = ?1 AND steps.task_id = ?2
            "#,
        )
        .bind(step_id)
        .bind(task_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| DeploymentRetentionError::NotFound("任务步骤不存在".to_owned()))?;
        Ok(PersistedStepLog {
            head: row.head.unwrap_or_default(),
            tail: row.tail.unwrap_or_default(),
            received_bytes: row.received_bytes.unwrap_or(0).max(0) as u64,
            dropped_bytes: row.dropped_bytes.unwrap_or(0).max(0) as u64,
            head_limit: row.head_limit.unwrap_or(self.head_limit as i64).max(0) as usize,
            tail_limit: row.tail_limit.unwrap_or(self.tail_limit as i64).max(0) as usize,
            finished: row.finished.unwrap_or(false),
            updated_at: row.log_updated_at.unwrap_or(row.step_updated_at),
        })
    }

    async fn persistence_snapshot(
        &self,
        task_id: i64,
        step_id: i64,
    ) -> Option<LogPersistenceSnapshot> {
        let task = self.active_task_if_present(task_id).await?;
        let task = task.lock().await;
        let step = task.steps.get(&step_id)?;
        Some(LogPersistenceSnapshot {
            step: step.buffer.snapshot(),
            head_limit: step.buffer.head_limit,
            tail_limit: step.buffer.tail_limit,
            task_max_bytes: task.budget.max_bytes,
            task_stored_bytes: task.budget.stored_bytes() as u64,
            task_received_bytes: task.budget.received_bytes(),
            task_dropped_bytes: task.budget.dropped_bytes(),
            task_truncated: task.budget.truncated(),
            finished: step.finished,
        })
    }

    async fn persist(
        &self,
        task_id: i64,
        step_id: i64,
        snapshot: &LogPersistenceSnapshot,
    ) -> Result<(), DeploymentRetentionError> {
        let mut tx = self.db.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO deployment_task_log_budgets(
                task_id, stored_bytes, received_bytes, dropped_bytes, max_bytes, truncated
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(task_id) DO UPDATE SET
                stored_bytes = MAX(deployment_task_log_budgets.stored_bytes, excluded.stored_bytes),
                received_bytes = MAX(deployment_task_log_budgets.received_bytes, excluded.received_bytes),
                dropped_bytes = MAX(deployment_task_log_budgets.dropped_bytes, excluded.dropped_bytes),
                max_bytes = excluded.max_bytes,
                truncated = MAX(deployment_task_log_budgets.truncated, excluded.truncated),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(task_id)
        .bind(snapshot.task_stored_bytes as i64)
        .bind(snapshot.task_received_bytes as i64)
        .bind(snapshot.task_dropped_bytes as i64)
        .bind(snapshot.task_max_bytes as i64)
        .bind(snapshot.task_truncated)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO deployment_step_log_buffers(
                step_id, task_id, head_content, tail_content, stored_bytes,
                received_bytes, dropped_bytes, head_limit_bytes, tail_limit_bytes,
                truncated, finished
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(step_id) DO UPDATE SET
                head_content = excluded.head_content,
                tail_content = excluded.tail_content,
                stored_bytes = excluded.stored_bytes,
                received_bytes = excluded.received_bytes,
                dropped_bytes = excluded.dropped_bytes,
                truncated = excluded.truncated,
                finished = MAX(deployment_step_log_buffers.finished, excluded.finished),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE excluded.received_bytes >= deployment_step_log_buffers.received_bytes
            "#,
        )
        .bind(step_id)
        .bind(task_id)
        .bind(&snapshot.step.head)
        .bind(&snapshot.step.tail)
        .bind(snapshot.step.stored_bytes as i64)
        .bind(snapshot.step.received_bytes as i64)
        .bind(snapshot.step.dropped_bytes as i64)
        .bind(snapshot.head_limit as i64)
        .bind(snapshot.tail_limit as i64)
        .bind(snapshot.step.truncated)
        .bind(snapshot.finished)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

impl DeploymentRetentionService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn preview_artifact_cleanup(
        &self,
        unit_release_id: i64,
    ) -> Result<ArtifactCleanupPreview, DeploymentRetentionError> {
        let mut tx = self.db.begin().await?;
        let preview = artifact_cleanup_preview(&mut tx, unit_release_id).await?;
        tx.commit().await?;
        Ok(preview)
    }

    pub async fn preview_application_release_cleanup(
        &self,
        app_id: i64,
        app_release_id: i64,
    ) -> Result<ApplicationReleaseCleanupPreview, DeploymentRetentionError> {
        let mut tx = self.db.begin().await?;
        let preview = application_release_cleanup_preview(&mut tx, app_id, app_release_id).await?;
        tx.commit().await?;
        Ok(preview)
    }

    pub async fn preview_deployment_history_delete(
        &self,
        app_id: i64,
        deployment_run_id: i64,
    ) -> Result<DeploymentHistoryDeletePreview, DeploymentRetentionError> {
        let mut tx = self.db.begin().await?;
        let preview = deployment_history_delete_preview(&mut tx, app_id, deployment_run_id).await?;
        tx.commit().await?;
        Ok(preview)
    }

    pub async fn delete_deployment_history(
        &self,
        app_id: i64,
        deployment_run_id: i64,
        operator: &str,
    ) -> Result<DeploymentHistoryDeleteResult, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let preview = deployment_history_delete_preview(&mut tx, app_id, deployment_run_id).await?;
        if !preview.allowed() {
            return Err(DeploymentRetentionError::InvalidState(format!(
                "部署历史当前不可彻底删除：{}",
                preview.blockers.join("；")
            )));
        }

        let mut deleted_queue_rows = 0;
        let mut cleared_task_release = false;
        if let Some(task_id) = preview.task_id {
            let deleted_queue = sqlx::query(
                r#"
                DELETE FROM app_release_queue
                WHERE task_id = ?1
                  AND release_id = ?2
                  AND status NOT IN ('scheduled', 'queued', 'running')
                "#,
            )
            .bind(task_id)
            .bind(preview.app_release_id)
            .execute(&mut *tx)
            .await?;
            deleted_queue_rows = deleted_queue.rows_affected();

            let cleared_task = sqlx::query(
                "UPDATE operation_tasks SET release_id = NULL WHERE id = ?1 AND release_id = ?2",
            )
            .bind(task_id)
            .bind(preview.app_release_id)
            .execute(&mut *tx)
            .await?;
            cleared_task_release = cleared_task.rows_affected() > 0;
        }

        insert_cleanup_event(
            &mut tx,
            "environment_deployment_run",
            deployment_run_id,
            "warning",
            "部署历史已彻底删除",
            operator,
            &format!(
                "删除 {} 条部署单元结果，应用版本 #{} 的任务引用已{}清除，关联发布队列删除 {} 条",
                preview.unit_result_count,
                preview.app_release_id,
                if cleared_task_release { "" } else { "无需" },
                deleted_queue_rows
            ),
        )
        .await?;

        let deleted_run =
            sqlx::query("DELETE FROM environment_deployment_runs WHERE id = ?1 AND app_id = ?2")
                .bind(deployment_run_id)
                .bind(app_id)
                .execute(&mut *tx)
                .await?;
        if deleted_run.rows_affected() != 1 {
            return Err(DeploymentRetentionError::NotFound(
                "部署历史不存在".to_owned(),
            ));
        }
        tx.commit().await?;

        Ok(DeploymentHistoryDeleteResult {
            deployment_run_id,
            app_release_id: preview.app_release_id,
            task_id: preview.task_id,
            deleted_unit_results: preview.unit_result_count.max(0) as u64,
            deleted_queue_rows,
            cleared_task_release,
        })
    }

    pub async fn archive_application_release(
        &self,
        app_id: i64,
        app_release_id: i64,
        operator: &str,
    ) -> Result<ApplicationReleaseCleanupPreview, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let preview = application_release_cleanup_preview(&mut tx, app_id, app_release_id).await?;
        if !preview.can_archive() {
            return Err(DeploymentRetentionError::InvalidState(format!(
                "应用版本当前不可归档：{}",
                if preview.archive_blockers.is_empty() {
                    format!("当前状态为 {}", preview.immutable_status)
                } else {
                    preview.archive_blockers.join("；")
                }
            )));
        }
        sqlx::query(
            r#"
            UPDATE application_release_manifests
            SET immutable_status = 'archived',
                archived_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE app_release_id = ?1 AND immutable_status = 'ready'
            "#,
        )
        .bind(app_release_id)
        .execute(&mut *tx)
        .await?;
        insert_cleanup_event(
            &mut tx,
            "application_release",
            app_release_id,
            "info",
            "应用版本已归档",
            operator,
            &format!(
                "版本 {} · versionCode {}",
                preview.version, preview.version_code
            ),
        )
        .await?;
        let archived = application_release_cleanup_preview(&mut tx, app_id, app_release_id).await?;
        tx.commit().await?;
        Ok(archived)
    }

    pub async fn delete_application_release(
        &self,
        app_id: i64,
        app_release_id: i64,
        operator: &str,
    ) -> Result<u64, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let preview = application_release_cleanup_preview(&mut tx, app_id, app_release_id).await?;
        if !preview.can_delete() {
            return Err(DeploymentRetentionError::InvalidState(format!(
                "应用版本当前不可彻底删除：{}",
                if preview.blockers.is_empty() {
                    format!("请先归档，当前状态为 {}", preview.immutable_status)
                } else {
                    preview.blockers.join("；")
                }
            )));
        }
        insert_cleanup_event(
            &mut tx,
            "application_release",
            app_release_id,
            "info",
            "已彻底删除应用版本",
            operator,
            &format!(
                "版本 {} · versionCode {}，结构化部署历史和审计记录未删除",
                preview.version, preview.version_code
            ),
        )
        .await?;
        let deleted = sqlx::query("DELETE FROM app_releases WHERE id = ?1 AND app_id = ?2")
            .bind(app_release_id)
            .bind(app_id)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() != 1 {
            return Err(DeploymentRetentionError::NotFound(
                "应用版本不存在".to_owned(),
            ));
        }
        tx.commit().await?;
        Ok(preview.estimated_bytes)
    }

    pub async fn cleanup_artifact(
        &self,
        unit_release_id: i64,
        operator: &str,
        deleter: &dyn ArtifactObjectDeleter,
    ) -> Result<ArtifactCleanupResult, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let preview = artifact_cleanup_preview(&mut tx, unit_release_id).await?;
        if !preview.allowed() {
            return Err(DeploymentRetentionError::InvalidState(format!(
                "制品当前不可清理：{}",
                if preview.blockers.is_empty() {
                    preview.artifact_status
                } else {
                    preview.blockers.join("；")
                }
            )));
        }
        let target = sqlx::query_as::<_, (String, String, String, String, String, String, String)>(
            r#"
            SELECT storage_provider, package_path, extract_dir, storage_bucket,
                   storage_endpoint, storage_object_key, storage_object_version_id
            FROM deployment_unit_releases WHERE id = ?1
            "#,
        )
        .bind(unit_release_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE deployment_unit_releases SET artifact_status = 'deleting', cleanup_error = '', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
        )
        .bind(unit_release_id)
        .execute(&mut *tx)
        .await?;
        insert_cleanup_event(
            &mut tx,
            "deployment_artifact",
            unit_release_id,
            "info",
            "开始清理部署单元制品",
            operator,
            "",
        )
        .await?;
        tx.commit().await?;

        let target = ArtifactDeletionTarget {
            provider: target.0,
            package_path: target.1,
            extract_dir: target.2,
            bucket: target.3,
            endpoint: target.4,
            object_key: target.5,
            object_version_id: target.6,
        };
        match deleter.delete(&target).await {
            Ok(()) => {
                let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
                sqlx::query(
                    r#"
                    UPDATE deployment_unit_releases
                    SET artifact_status = 'deleted', cleanup_error = '', package_path = '',
                        extract_dir = '', storage_bucket = '', storage_object_key = '',
                        storage_endpoint = '', storage_object_version_id = '',
                        updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                    WHERE id = ?1 AND artifact_status = 'deleting'
                    "#,
                )
                .bind(unit_release_id)
                .execute(&mut *tx)
                .await?;
                insert_cleanup_event(
                    &mut tx,
                    "deployment_artifact",
                    unit_release_id,
                    "info",
                    "部署单元制品清理完成",
                    operator,
                    "",
                )
                .await?;
                tx.commit().await?;
                Ok(ArtifactCleanupResult {
                    unit_release_id,
                    status: "deleted".to_owned(),
                    error: String::new(),
                })
            }
            Err(error) => {
                let error = truncate_cleanup_error(&error);
                let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
                sqlx::query(
                    "UPDATE deployment_unit_releases SET artifact_status = 'delete_failed', cleanup_error = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1 AND artifact_status = 'deleting'",
                )
                .bind(unit_release_id)
                .bind(&error)
                .execute(&mut *tx)
                .await?;
                insert_cleanup_event(
                    &mut tx,
                    "deployment_artifact",
                    unit_release_id,
                    "error",
                    "部署单元制品清理失败",
                    operator,
                    &error,
                )
                .await?;
                tx.commit().await?;
                Ok(ArtifactCleanupResult {
                    unit_release_id,
                    status: "delete_failed".to_owned(),
                    error,
                })
            }
        }
    }

    pub async fn recover_deleting_artifacts(
        &self,
        operator: &str,
        deleter: &dyn ArtifactObjectDeleter,
    ) -> Result<Vec<ArtifactCleanupResult>, DeploymentRetentionError> {
        let ids = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM deployment_unit_releases WHERE artifact_status = 'deleting' ORDER BY id",
        )
        .fetch_all(&self.db)
        .await?;
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            results.push(self.cleanup_artifact(id, operator, deleter).await?);
        }
        Ok(results)
    }

    pub async fn delete_deployment_snapshot(
        &self,
        deployment_run_id: i64,
        operator: &str,
    ) -> Result<u64, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let status: String = sqlx::query_scalar(
            "SELECT status FROM environment_deployment_runs WHERE id = ?1 AND snapshot_status = 'active'",
        )
        .bind(deployment_run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            DeploymentRetentionError::NotFound("部署快照不存在或已经删除".to_owned())
        })?;
        if matches!(status.as_str(), "queued" | "running" | "reconciling") {
            return Err(DeploymentRetentionError::InvalidState(
                "活动部署不能删除快照".to_owned(),
            ));
        }
        let bytes: i64 = sqlx::query_scalar(
            "SELECT length(CAST(plan_json AS BLOB)) FROM environment_deployment_runs WHERE id = ?1",
        )
        .bind(deployment_run_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE deployment_unit_run_results SET unit_release_id = NULL WHERE deployment_run_id = ?1",
        )
        .bind(deployment_run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            UPDATE environment_deployment_runs
            SET plan_json = '{}', plan_hash = '', snapshot_status = 'deleted',
                snapshot_deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(deployment_run_id)
        .execute(&mut *tx)
        .await?;
        insert_cleanup_event(
            &mut tx,
            "environment_deployment_run",
            deployment_run_id,
            "info",
            "部署配置与制品快照已清理",
            operator,
            "结构化部署结果继续保留",
        )
        .await?;
        tx.commit().await?;
        Ok(bytes.max(0) as u64)
    }
}

async fn artifact_cleanup_preview(
    tx: &mut Transaction<'_, Sqlite>,
    unit_release_id: i64,
) -> Result<ArtifactCleanupPreview, DeploymentRetentionError> {
    let release = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT version, size_bytes, artifact_status FROM deployment_unit_releases WHERE id = ?1",
    )
    .bind(unit_release_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| DeploymentRetentionError::NotFound("部署单元制品不存在".to_owned()))?;
    let mut blockers = Vec::new();
    let app_release_refs: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM app_release_units units
        JOIN application_release_manifests manifests
          ON manifests.app_release_id = units.app_release_id
        WHERE units.unit_release_id = ?1 AND manifests.immutable_status <> 'deleted'
        "#,
    )
    .bind(unit_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if app_release_refs > 0 {
        blockers.push(format!("仍被 {app_release_refs} 个应用版本引用"));
    }
    let runtime_refs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployment_unit_runtime_states WHERE active_unit_release_id = ?1",
    )
    .bind(unit_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if runtime_refs > 0 {
        blockers.push(format!("仍被 {runtime_refs} 个环境运行状态引用"));
    }
    let history_refs: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM deployment_unit_run_results results
        JOIN environment_deployment_runs runs ON runs.id = results.deployment_run_id
        WHERE results.unit_release_id = ?1 AND runs.snapshot_status = 'active'
        "#,
    )
    .bind(unit_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if history_refs > 0 {
        blockers.push(format!("仍被 {history_refs} 条可重放部署快照引用"));
    }
    Ok(ArtifactCleanupPreview {
        unit_release_id,
        version: release.0,
        size_bytes: release.1.max(0) as u64,
        artifact_status: release.2,
        blockers,
    })
}

async fn application_release_cleanup_preview(
    tx: &mut Transaction<'_, Sqlite>,
    app_id: i64,
    app_release_id: i64,
) -> Result<ApplicationReleaseCleanupPreview, DeploymentRetentionError> {
    let release = sqlx::query_as::<_, (String, i64, String, i64)>(
        r#"
        SELECT releases.version, releases.version_code, manifests.immutable_status,
               length(CAST(manifests.manifest_json AS BLOB))
                 + length(CAST(releases.metadata AS BLOB)) AS estimated_bytes
        FROM app_releases releases
        JOIN application_release_manifests manifests
          ON manifests.app_release_id = releases.id
        WHERE releases.id = ?1 AND releases.app_id = ?2
          AND manifests.immutable_status <> 'deleted'
        "#,
    )
    .bind(app_release_id)
    .bind(app_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| DeploymentRetentionError::NotFound("应用版本不存在".to_owned()))?;

    let mut archive_blockers = Vec::new();
    let mut blockers = Vec::new();
    let current_refs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM app_environments WHERE current_app_release_id = ?1",
    )
    .bind(app_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if current_refs > 0 {
        let blocker = format!("仍是 {current_refs} 个环境的当前版本");
        archive_blockers.push(blocker.clone());
        blockers.push(blocker);
    }
    let active_runs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_deployment_runs WHERE app_release_id = ?1 AND status IN ('queued', 'running', 'reconciling')",
    )
    .bind(app_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if active_runs > 0 {
        archive_blockers.push(format!("仍被 {active_runs} 个活动部署使用"));
    }
    let deployment_runs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM environment_deployment_runs WHERE app_release_id = ?1",
    )
    .bind(app_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if deployment_runs > 0 {
        blockers.push(format!("仍被 {deployment_runs} 条部署历史引用"));
    }
    let active_queue: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM app_release_queue WHERE release_id = ?1 AND status IN ('scheduled', 'queued', 'running')",
    )
    .bind(app_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if active_queue > 0 {
        archive_blockers.push(format!("仍有 {active_queue} 条活动发布队列记录"));
    }
    let queue_refs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM app_release_queue WHERE release_id = ?1")
            .bind(app_release_id)
            .fetch_one(&mut **tx)
            .await?;
    if queue_refs > 0 {
        blockers.push(format!("仍被 {queue_refs} 条发布队列历史引用"));
    }
    let task_refs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM operation_tasks WHERE release_id = ?1")
            .bind(app_release_id)
            .fetch_one(&mut **tx)
            .await?;
    if task_refs > 0 {
        blockers.push(format!("仍被 {task_refs} 条任务历史引用"));
    }
    let legacy_run_refs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deployment_runs WHERE release_id = ?1")
            .bind(app_release_id)
            .fetch_one(&mut **tx)
            .await?;
    if legacy_run_refs > 0 {
        blockers.push(format!("仍被 {legacy_run_refs} 条兼容部署历史引用"));
    }
    let base_refs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM application_release_manifests WHERE base_app_release_id = ?1 AND immutable_status <> 'deleted'",
    )
    .bind(app_release_id)
    .fetch_one(&mut **tx)
    .await?;
    if base_refs > 0 {
        blockers.push(format!("仍是 {base_refs} 个应用版本的继承基线"));
    }

    Ok(ApplicationReleaseCleanupPreview {
        app_release_id,
        version: release.0,
        version_code: release.1,
        immutable_status: release.2,
        estimated_bytes: release.3.max(0) as u64,
        archive_blockers,
        blockers,
    })
}

async fn deployment_history_delete_preview(
    tx: &mut Transaction<'_, Sqlite>,
    app_id: i64,
    deployment_run_id: i64,
) -> Result<DeploymentHistoryDeletePreview, DeploymentRetentionError> {
    let run = sqlx::query_as::<_, (i64, Option<i64>, String, String)>(
        r#"
        SELECT app_release_id, task_id, status, snapshot_status
        FROM environment_deployment_runs
        WHERE id = ?1 AND app_id = ?2
        "#,
    )
    .bind(deployment_run_id)
    .bind(app_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| DeploymentRetentionError::NotFound("部署历史不存在".to_owned()))?;

    let unit_result_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deployment_unit_run_results WHERE deployment_run_id = ?1",
    )
    .bind(deployment_run_id)
    .fetch_one(&mut **tx)
    .await?;

    let (log_reference_count, queue_reference_count) = match run.1 {
        Some(task_id) => {
            let log_references: i64 = sqlx::query_scalar(
                r#"
                SELECT
                    (SELECT COUNT(*) FROM deployment_step_log_buffers WHERE task_id = ?1)
                  + (SELECT COUNT(*) FROM deployment_task_log_budgets WHERE task_id = ?1)
                  + (SELECT COUNT(*) FROM operation_task_logs WHERE task_id = ?1)
                "#,
            )
            .bind(task_id)
            .fetch_one(&mut **tx)
            .await?;
            let active_queue_references: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM app_release_queue
                WHERE task_id = ?1
                  AND release_id = ?2
                  AND status IN ('scheduled', 'queued', 'running')
                "#,
            )
            .bind(task_id)
            .bind(run.0)
            .fetch_one(&mut **tx)
            .await?;
            (log_references, active_queue_references)
        }
        None => (0, 0),
    };

    let mut blockers = Vec::new();
    if matches!(run.2.as_str(), "queued" | "running" | "reconciling") {
        blockers.push(format!("部署仍处于 {} 状态", run.2));
    }
    if run.3 != "deleted" {
        blockers.push("请先清理配置与制品快照".to_owned());
    }
    if log_reference_count > 0 {
        blockers.push("请先清理执行日志".to_owned());
    }
    if queue_reference_count > 0 {
        blockers.push("关联发布队列仍处于活动状态".to_owned());
    }

    Ok(DeploymentHistoryDeletePreview {
        deployment_run_id,
        app_release_id: run.0,
        task_id: run.1,
        status: run.2,
        snapshot_status: run.3,
        log_reference_count,
        unit_result_count,
        queue_reference_count,
        blockers,
    })
}

async fn insert_cleanup_event(
    tx: &mut Transaction<'_, Sqlite>,
    target_type: &str,
    target_id: i64,
    level: &str,
    title: &str,
    operator: &str,
    detail: &str,
) -> Result<(), DeploymentRetentionError> {
    sqlx::query(
        r#"
        INSERT INTO event_logs(
            event_type, level, target_type, target_id, target_name, title, summary, detail
        ) VALUES ('deployment.cleanup', ?1, ?2, ?3, '', ?4, ?5, ?6)
        "#,
    )
    .bind(level)
    .bind(target_type)
    .bind(target_id.to_string())
    .bind(title)
    .bind(format!("操作人：{}", operator.trim()))
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn delete_local_artifact_paths<'a>(
    data_dir: &Path,
    paths: impl IntoIterator<Item = &'a String>,
) -> Result<(), String> {
    let root = tokio::fs::canonicalize(data_dir).await.map_err(|error| {
        format!(
            "无法确认平台数据目录 {}：{error}",
            data_dir.to_string_lossy()
        )
    })?;
    let mut targets = Vec::new();
    for path in paths {
        let value = path.trim();
        if value.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(value);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            data_dir.join(candidate)
        };
        let metadata = match tokio::fs::symlink_metadata(&candidate).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "读取本地制品路径 {} 失败：{error}",
                    candidate.to_string_lossy()
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "本地制品路径 {} 是符号链接，拒绝清理",
                candidate.to_string_lossy()
            ));
        }
        let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
            format!(
                "无法确认本地制品路径 {}：{error}",
                candidate.to_string_lossy()
            )
        })?;
        if canonical == root || !canonical.starts_with(&root) {
            return Err(format!(
                "本地制品路径 {} 不在平台数据目录内，拒绝清理",
                candidate.to_string_lossy()
            ));
        }
        targets.push((candidate, metadata));
    }

    for (target, metadata) in targets {
        if metadata.is_dir() {
            tokio::fs::remove_dir_all(&target).await
        } else if metadata.is_file() {
            tokio::fs::remove_file(&target).await
        } else {
            return Err(format!(
                "本地制品路径 {} 不是普通文件或目录，拒绝清理",
                target.to_string_lossy()
            ));
        }
        .map_err(|error| {
            format!(
                "删除本地制品路径 {} 失败：{error}",
                target.to_string_lossy()
            )
        })?;
    }
    Ok(())
}

fn truncate_cleanup_error(error: &str) -> String {
    error.chars().take(1000).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedLogSnapshot {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
    pub stored_bytes: u64,
    pub received_bytes: u64,
    pub dropped_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct TaskLogBudget {
    max_bytes: usize,
    stored_bytes: usize,
    received_bytes: u64,
    dropped_bytes: u64,
}

impl TaskLogBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            stored_bytes: 0,
            received_bytes: 0,
            dropped_bytes: 0,
        }
    }

    fn from_usage(
        max_bytes: usize,
        stored_bytes: usize,
        received_bytes: u64,
        dropped_bytes: u64,
    ) -> Self {
        Self {
            max_bytes,
            stored_bytes: stored_bytes.min(max_bytes),
            received_bytes,
            dropped_bytes,
        }
    }

    fn allocate(&mut self, requested: usize) -> usize {
        let allocated = requested.min(self.max_bytes.saturating_sub(self.stored_bytes));
        self.stored_bytes += allocated;
        allocated
    }

    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }

    pub fn received_bytes(&self) -> u64 {
        self.received_bytes
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    pub fn truncated(&self) -> bool {
        self.dropped_bytes > 0
    }
}

#[derive(Debug)]
pub struct BoundedLogBuffer {
    head_limit: usize,
    tail_limit: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    received_bytes: u64,
    dropped_bytes: u64,
    redactor: StreamingRedactor,
}

impl BoundedLogBuffer {
    pub fn new(head_limit: usize, tail_limit: usize, secrets: Vec<String>) -> Self {
        Self {
            head_limit,
            tail_limit,
            head: Vec::with_capacity(head_limit.min(64 * 1024)),
            tail: VecDeque::with_capacity(tail_limit.min(64 * 1024)),
            received_bytes: 0,
            dropped_bytes: 0,
            redactor: StreamingRedactor::new(secrets),
        }
    }

    fn from_persisted(
        head_limit: usize,
        tail_limit: usize,
        mut head: Vec<u8>,
        tail: Vec<u8>,
        received_bytes: u64,
        dropped_bytes: u64,
        secrets: Vec<String>,
    ) -> Self {
        head.truncate(head_limit);
        let tail_start = tail.len().saturating_sub(tail_limit);
        Self {
            head_limit,
            tail_limit,
            head,
            tail: tail[tail_start..].iter().copied().collect(),
            received_bytes,
            dropped_bytes,
            redactor: StreamingRedactor::new(secrets),
        }
    }

    pub fn append(&mut self, chunk: &[u8], task_budget: &mut TaskLogBudget) {
        let redacted = self.redactor.push(chunk);
        self.append_redacted(&redacted, task_budget);
    }

    pub fn finish(&mut self, task_budget: &mut TaskLogBudget) {
        let redacted = self.redactor.finish();
        self.append_redacted(&redacted, task_budget);
    }

    pub fn record_external_drop(&mut self, count: u64, task_budget: &mut TaskLogBudget) {
        self.received_bytes = self.received_bytes.saturating_add(count);
        task_budget.received_bytes = task_budget.received_bytes.saturating_add(count);
        self.record_drop(count, task_budget);
    }

    pub fn snapshot(&self) -> BoundedLogSnapshot {
        BoundedLogSnapshot {
            head: self.head.clone(),
            tail: self.tail.iter().copied().collect(),
            stored_bytes: (self.head.len() + self.tail.len()) as u64,
            received_bytes: self.received_bytes,
            dropped_bytes: self.dropped_bytes,
            truncated: self.dropped_bytes > 0,
        }
    }

    fn preview(&self, head_limit: usize, tail_limit: usize) -> BoundedTaskLogPreview {
        let head_len = self.head.len().min(head_limit);
        let tail_start = self.tail.len().saturating_sub(tail_limit);
        let head = self.head[..head_len].to_vec();
        let tail = self
            .tail
            .iter()
            .skip(tail_start)
            .copied()
            .collect::<Vec<_>>();
        let stored_bytes = self.head.len() + self.tail.len();
        BoundedTaskLogPreview {
            step_id: 0,
            preview_omitted_bytes: stored_bytes.saturating_sub(head.len() + tail.len()) as u64,
            head,
            tail,
            stored_bytes: stored_bytes as u64,
            dropped_bytes: self.dropped_bytes,
            truncated: self.dropped_bytes > 0,
            updated_at: String::new(),
            live: true,
        }
    }

    fn append_redacted(&mut self, bytes: &[u8], task_budget: &mut TaskLogBudget) {
        if bytes.is_empty() {
            return;
        }
        self.received_bytes += bytes.len() as u64;
        task_budget.received_bytes += bytes.len() as u64;
        let mut offset = 0;
        if self.head.len() < self.head_limit {
            let requested = (self.head_limit - self.head.len()).min(bytes.len());
            let allocated = task_budget.allocate(requested);
            self.head.extend_from_slice(&bytes[..allocated]);
            offset += allocated;
            if allocated < requested {
                self.record_drop((requested - allocated) as u64, task_budget);
                offset += requested - allocated;
            }
        }
        let mut remaining = &bytes[offset..];
        if self.tail.len() < self.tail_limit && !remaining.is_empty() {
            let requested = (self.tail_limit - self.tail.len()).min(remaining.len());
            let allocated = task_budget.allocate(requested);
            self.tail.extend(remaining[..allocated].iter().copied());
            remaining = &remaining[allocated..];
        }
        if remaining.is_empty() {
            return;
        }
        let retained_tail = self.tail.len();
        if retained_tail == 0 {
            self.record_drop(remaining.len() as u64, task_budget);
            return;
        }
        if remaining.len() >= retained_tail {
            self.tail.clear();
            self.tail
                .extend(remaining[remaining.len() - retained_tail..].iter().copied());
        } else {
            self.tail.drain(..remaining.len());
            self.tail.extend(remaining.iter().copied());
        }
        self.record_drop(remaining.len() as u64, task_budget);
    }

    fn record_drop(&mut self, count: u64, task_budget: &mut TaskLogBudget) {
        self.dropped_bytes += count;
        task_budget.dropped_bytes += count;
    }
}

pub fn redact_log_text(secrets: &[String], content: &str) -> String {
    let mut redactor = StreamingRedactor::new(secrets.to_vec());
    let mut redacted = redactor.push(content.as_bytes());
    redacted.extend(redactor.finish());
    String::from_utf8_lossy(&redacted).into_owned()
}

#[derive(Debug)]
pub(crate) struct StreamingRedactor {
    secrets: Vec<Vec<u8>>,
    pending: Vec<u8>,
    overlap: usize,
    masking_assignment: bool,
}

impl StreamingRedactor {
    pub(crate) fn new(secrets: Vec<String>) -> Self {
        let mut secrets = secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .map(String::into_bytes)
            .collect::<Vec<_>>();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        let overlap = secrets
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(1)
            .saturating_sub(1)
            .max(256);
        Self {
            secrets,
            pending: Vec::new(),
            overlap,
            masking_assignment: false,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        self.process(false)
    }

    pub(crate) fn finish(&mut self) -> Vec<u8> {
        self.process(true)
    }

    fn process(&mut self, final_chunk: bool) -> Vec<u8> {
        let safe_end = if final_chunk {
            self.pending.len()
        } else {
            self.pending.len().saturating_sub(self.overlap)
        };
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < safe_end {
            if self.masking_assignment {
                if is_assignment_terminator(self.pending[offset]) {
                    self.masking_assignment = false;
                    output.push(self.pending[offset]);
                }
                offset += 1;
                continue;
            }
            if let Some(secret_len) = self
                .secrets
                .iter()
                .find(|secret| self.pending[offset..].starts_with(secret))
                .map(Vec::len)
            {
                output.extend_from_slice(b"[REDACTED]");
                offset += secret_len;
                continue;
            }
            if let Some(prefix_len) = sensitive_assignment_prefix(&self.pending[offset..]) {
                if offset + prefix_len > safe_end && !final_chunk {
                    break;
                }
                output.extend_from_slice(&self.pending[offset..offset + prefix_len]);
                output.extend_from_slice(b"[REDACTED]");
                self.masking_assignment = true;
                offset += prefix_len;
                continue;
            }
            output.push(self.pending[offset]);
            offset += 1;
        }
        self.pending.drain(..offset);
        output
    }
}

fn sensitive_assignment_prefix(value: &[u8]) -> Option<usize> {
    const KEYS: [&[u8]; 4] = [b"password", b"token", b"secret", b"authorization"];
    for key in KEYS {
        if value.len() < key.len() || !value[..key.len()].eq_ignore_ascii_case(key) {
            continue;
        }
        let mut offset = key.len();
        while value.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if !value
            .get(offset)
            .is_some_and(|byte| matches!(byte, b'=' | b':'))
        {
            continue;
        }
        offset += 1;
        while value.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        return Some(offset);
    }
    None
}

fn is_assignment_terminator(byte: u8) -> bool {
    matches!(byte, b'\n' | b'\r' | b'\'' | b'"' | b',' | b'&')
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};

    use super::*;

    #[test]
    fn small_log_is_preserved_without_truncation() {
        let mut task = TaskLogBudget::new(100);
        let mut buffer = BoundedLogBuffer::new(10, 20, vec![]);
        buffer.append(b"hello", &mut task);
        buffer.finish(&mut task);

        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.head, b"hello");
        assert!(snapshot.tail.is_empty());
        assert!(!snapshot.truncated);
        assert_eq!(task.stored_bytes(), 5);
    }

    #[test]
    fn keeps_head_and_latest_tail_with_drop_counts() {
        let mut task = TaskLogBudget::new(10);
        let mut buffer = BoundedLogBuffer::new(4, 6, vec![]);
        buffer.append(b"abcdefghijklmnop", &mut task);
        buffer.finish(&mut task);

        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.head, b"abcd");
        assert_eq!(snapshot.tail, b"klmnop");
        assert_eq!(snapshot.stored_bytes, 10);
        assert_eq!(snapshot.received_bytes, 16);
        assert_eq!(snapshot.dropped_bytes, 6);
        assert!(snapshot.truncated);
        assert_eq!(task.dropped_bytes(), 6);
    }

    #[test]
    fn task_budget_prevents_steps_from_growing_total_storage() {
        let mut task = TaskLogBudget::new(8);
        let mut first = BoundedLogBuffer::new(4, 4, vec![]);
        let mut second = BoundedLogBuffer::new(4, 4, vec![]);
        first.append(b"12345678", &mut task);
        second.append(b"abcdefgh", &mut task);
        first.finish(&mut task);
        second.finish(&mut task);

        assert_eq!(task.stored_bytes(), 8);
        assert_eq!(task.received_bytes(), 16);
        assert_eq!(task.dropped_bytes(), 8);
        assert!(task.truncated());
        assert_eq!(second.snapshot().stored_bytes, 0);
    }

    #[test]
    fn redacts_secret_split_across_chunks_and_common_assignments() {
        let mut task = TaskLogBudget::new(1024);
        let mut buffer = BoundedLogBuffer::new(1024, 0, vec!["very-sensitive-value".to_owned()]);
        buffer.append(b"value=very-sensi", &mut task);
        buffer.append(b"tive-value\npassword=hunter2\n", &mut task);
        buffer.finish(&mut task);
        let content = String::from_utf8(buffer.snapshot().head).expect("utf8 log");

        assert!(!content.contains("very-sensitive-value"));
        assert!(!content.contains("hunter2"));
        assert!(content.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_secret_crossing_stream_processing_boundary() {
        let secret = "boundary-secret-value";
        let mut task = TaskLogBudget::new(4096);
        let mut buffer = BoundedLogBuffer::new(4096, 0, vec![secret.to_owned()]);
        let mut first = vec![b'x'; 35];
        first.extend_from_slice(secret.as_bytes());
        first.extend_from_slice(&vec![b'y'; 244]);
        buffer.append(&first, &mut task);
        buffer.finish(&mut task);
        let content = String::from_utf8(buffer.snapshot().head).expect("utf8 log");

        assert!(!content.contains(secret));
        assert!(!content.contains("boundary-se"));
        assert!(content.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn persists_bounded_snapshot_and_deletes_only_log_content() {
        let db = SqlitePool::connect_with(
            "sqlite::memory:"
                .parse::<SqliteConnectOptions>()
                .expect("sqlite options")
                .foreign_keys(true),
        )
        .await
        .expect("connect sqlite");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("run migrations");
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, status) VALUES ('release.deploy', 'test', 'running')",
        )
        .execute(&db)
        .await
        .expect("insert task")
        .last_insert_rowid();
        let step_id = sqlx::query(
            "INSERT INTO operation_task_steps(task_id, step_no, step_key, title, status) VALUES (?1, 1, 'unit', 'unit', 'running')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert step")
        .last_insert_rowid();
        let service = DeploymentLogService::with_limits(db.clone(), 16, 16, 32);
        service
            .append(
                task_id,
                step_id,
                &["explicit-secret".to_owned()],
                b"token=unknown-token\nexplicit-",
            )
            .await
            .expect("append first chunk");
        service
            .append(task_id, step_id, &[], b"secret\n0123456789abcdef")
            .await
            .expect("append second chunk");
        service.finish(task_id, step_id).await.expect("finish log");

        let snapshot = service
            .snapshot(task_id, step_id)
            .await
            .expect("load snapshot");
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&snapshot.head),
            String::from_utf8_lossy(&snapshot.tail)
        );
        assert!(!rendered.contains("unknown-token"));
        assert!(!rendered.contains("explicit-secret"));
        assert!(snapshot.stored_bytes <= 32);

        let active_error = service
            .delete_task_logs(task_id, "operator")
            .await
            .expect_err("active task logs stay protected");
        assert!(matches!(
            active_error,
            DeploymentRetentionError::InvalidState(_)
        ));

        sqlx::query("UPDATE operation_tasks SET status = 'success' WHERE id = ?1")
            .bind(task_id)
            .execute(&db)
            .await
            .expect("finish task");

        let released = service
            .delete_task_logs(task_id, "operator")
            .await
            .expect("delete logs");
        assert_eq!(released, snapshot.stored_bytes);
        assert!(service.snapshot(task_id, step_id).await.is_err());
        let task_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operation_tasks WHERE id = ?1)")
                .bind(task_id)
                .fetch_one(&db)
                .await
                .expect("check task");
        let step_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operation_task_steps WHERE id = ?1)")
                .bind(step_id)
                .fetch_one(&db)
                .await
                .expect("check step");
        assert!(task_exists && step_exists);
        let cleanup_event: (String, String) = sqlx::query_as(
            "SELECT target_type, summary FROM event_logs WHERE event_type = 'deployment.cleanup' AND target_id = ?1 ORDER BY id DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .fetch_one(&db)
        .await
        .expect("load log cleanup event");
        assert_eq!(cleanup_event.0, "task");
        assert!(cleanup_event.1.contains("operator"));
    }

    #[tokio::test]
    async fn task_preview_is_bounded_and_finished_step_releases_live_buffer() {
        let db = retention_database().await;
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, status) VALUES ('release.deploy', 'preview', 'running')",
        )
        .execute(&db)
        .await
        .expect("insert task")
        .last_insert_rowid();
        let step_id = sqlx::query(
            "INSERT INTO operation_task_steps(task_id, step_no, step_key, title, status) VALUES (?1, 1, 'unit', 'unit', 'running')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert step")
        .last_insert_rowid();
        let service = DeploymentLogService::with_limits(db, 64, 64, 128);
        service
            .append_buffered(task_id, step_id, &[], &vec![b'x'; 1_000])
            .await
            .expect("buffer output");
        service.finish(task_id, step_id).await.expect("finish log");

        let previews = service
            .task_previews_with_limits(task_id, 4, 4, 32)
            .await
            .expect("load preview");
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].head.len(), 4);
        assert_eq!(previews[0].tail.len(), 4);
        assert_eq!(previews[0].preview_omitted_bytes, 120);
        assert!(previews[0].truncated);
        assert!(!previews[0].live);

        let task = service
            .active_task_if_present(task_id)
            .await
            .expect("task budget remains cached");
        assert!(!task.lock().await.steps.contains_key(&step_id));
    }

    #[tokio::test]
    async fn artifact_cleanup_blocks_references_and_retries_external_failure() {
        let db = retention_database().await;
        let app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('retention-app', 'Retention', 'compose', 'compose', '/srv/app', 'ready')",
        )
        .execute(&db)
        .await
        .expect("insert app")
        .last_insert_rowid();
        let unit_id = sqlx::query(
            "INSERT INTO deployment_units(app_id, unit_key, name) VALUES (?1, 'api', 'API')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert unit")
        .last_insert_rowid();
        let referenced_id = insert_artifact(&db, unit_id, "1.0.0", "/tmp/referenced.tgz").await;
        let app_release_id = sqlx::query(
            "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '1.0.0', 100, 'received', 'openapi')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert app release")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO application_release_manifests(app_release_id, manifest_hash, manifest_json) VALUES (?1, 'retention-manifest', '{}')",
        )
        .bind(app_release_id)
        .execute(&db)
        .await
        .expect("insert manifest");
        sqlx::query(
            "INSERT INTO app_release_units(app_release_id, unit_id, unit_release_id) VALUES (?1, ?2, ?3)",
        )
        .bind(app_release_id)
        .bind(unit_id)
        .bind(referenced_id)
        .execute(&db)
        .await
        .expect("insert release unit");
        let service = DeploymentRetentionService::new(db.clone());
        let blocked = service
            .preview_artifact_cleanup(referenced_id)
            .await
            .expect("preview referenced artifact");
        assert!(!blocked.allowed());
        assert!(blocked.blockers[0].contains("应用版本"));

        let retry_id = insert_artifact(&db, unit_id, "1.1.0", "/tmp/retry.tgz").await;
        let deleter = FailingOnceDeleter::default();
        let failed = service
            .cleanup_artifact(retry_id, "operator", &deleter)
            .await
            .expect("record external failure");
        assert_eq!(failed.status, "delete_failed");
        let retained_path: String =
            sqlx::query_scalar("SELECT package_path FROM deployment_unit_releases WHERE id = ?1")
                .bind(retry_id)
                .fetch_one(&db)
                .await
                .expect("load retained path");
        assert_eq!(retained_path, "/tmp/retry.tgz");

        let deleted = service
            .cleanup_artifact(retry_id, "operator", &deleter)
            .await
            .expect("retry cleanup");
        assert_eq!(deleted.status, "deleted");
        let persisted: (String, String) = sqlx::query_as(
            "SELECT artifact_status, package_path FROM deployment_unit_releases WHERE id = ?1",
        )
        .bind(retry_id)
        .fetch_one(&db)
        .await
        .expect("load deleted artifact");
        assert_eq!(persisted, ("deleted".to_owned(), String::new()));
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM event_logs WHERE event_type = 'deployment.cleanup' AND target_id = ?1",
        )
        .bind(retry_id.to_string())
        .fetch_one(&db)
        .await
        .expect("count cleanup audits");
        assert_eq!(audit_count, 4);
    }

    #[tokio::test]
    async fn application_release_archive_and_delete_honor_reference_blockers() {
        let db = retention_database().await;
        let app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('release-cleanup-app', 'Release Cleanup', 'compose', 'compose', '/srv/app', 'ready')",
        )
        .execute(&db)
        .await
        .expect("insert app")
        .last_insert_rowid();
        let environment_id = sqlx::query(
            "INSERT INTO app_environments(app_id, environment_key, name, status) VALUES (?1, 'production', '正式环境', 'ready')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert environment")
        .last_insert_rowid();
        let unit_id = sqlx::query(
            "INSERT INTO deployment_units(app_id, unit_key, name) VALUES (?1, 'api', 'API')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert unit")
        .last_insert_rowid();
        let artifact_id = insert_artifact(&db, unit_id, "3.0.0", "/tmp/release-cleanup.tgz").await;
        let app_release_id = sqlx::query(
            "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '3.0.0', 100, 'received', 'openapi')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert app release")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO application_release_manifests(app_release_id, manifest_hash, manifest_json) VALUES (?1, 'release-cleanup-manifest', '{\"units\":[\"api\"]}')",
        )
        .bind(app_release_id)
        .execute(&db)
        .await
        .expect("insert manifest");
        sqlx::query(
            "INSERT INTO app_release_units(app_release_id, unit_id, unit_release_id) VALUES (?1, ?2, ?3)",
        )
        .bind(app_release_id)
        .bind(unit_id)
        .bind(artifact_id)
        .execute(&db)
        .await
        .expect("insert release unit");
        sqlx::query("UPDATE app_environments SET current_app_release_id = ?2 WHERE id = ?1")
            .bind(environment_id)
            .bind(app_release_id)
            .execute(&db)
            .await
            .expect("set current release");
        let service = DeploymentRetentionService::new(db.clone());

        let blocked = service
            .preview_application_release_cleanup(app_id, app_release_id)
            .await
            .expect("preview current release");
        assert!(!blocked.can_archive());
        assert!(!blocked.can_delete());
        assert!(
            blocked
                .blockers
                .iter()
                .any(|blocker| blocker.contains("当前版本"))
        );

        sqlx::query("UPDATE app_environments SET current_app_release_id = NULL WHERE id = ?1")
            .bind(environment_id)
            .execute(&db)
            .await
            .expect("clear current release");
        let archived = service
            .archive_application_release(app_id, app_release_id, "operator")
            .await
            .expect("archive release");
        assert_eq!(archived.immutable_status, "archived");
        assert!(archived.can_delete());

        let released = service
            .delete_application_release(app_id, app_release_id, "operator")
            .await
            .expect("delete archived release");
        assert!(released > 0);
        let release_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM app_releases WHERE id = ?1)")
                .bind(app_release_id)
                .fetch_one(&db)
                .await
                .expect("check deleted release");
        assert!(!release_exists);
        let artifact = service
            .preview_artifact_cleanup(artifact_id)
            .await
            .expect("preview newly unreferenced artifact");
        assert!(artifact.allowed());
        let event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM event_logs WHERE event_type = 'deployment.cleanup' AND target_type = 'application_release' AND target_id = ?1",
        )
        .bind(app_release_id.to_string())
        .fetch_one(&db)
        .await
        .expect("count application release events");
        assert_eq!(event_count, 2);
    }

    #[tokio::test]
    async fn deleting_deployment_snapshot_preserves_structured_unit_result() {
        let db = retention_database().await;
        let app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('snapshot-app', 'Snapshot', 'compose', 'compose', '/srv/app', 'ready')",
        )
        .execute(&db)
        .await
        .expect("insert app")
        .last_insert_rowid();
        let environment_id = sqlx::query(
            "INSERT INTO app_environments(app_id, environment_key, name, status) VALUES (?1, 'production', '正式环境', 'ready')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert environment")
        .last_insert_rowid();
        let unit_id = sqlx::query(
            "INSERT INTO deployment_units(app_id, unit_key, name) VALUES (?1, 'api', 'API')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert unit")
        .last_insert_rowid();
        let artifact_id = insert_artifact(&db, unit_id, "2.0.0", "/tmp/snapshot.tgz").await;
        let config_id = sqlx::query(
            "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, config_hash) VALUES (?1, 100, '{}', '{}', 'snapshot-config')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert config")
        .last_insert_rowid();
        let app_release_id = sqlx::query(
            "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '2.0.0', 100, 'received', 'openapi')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert app release")
        .last_insert_rowid();
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, app_id, release_id, environment_id, status) VALUES ('release.deploy', 'snapshot', ?1, ?2, ?3, 'success')",
        )
        .bind(app_id)
        .bind(app_release_id)
        .bind(environment_id)
        .execute(&db)
        .await
        .expect("insert task")
        .last_insert_rowid();
        let run_id = sqlx::query(
            "INSERT INTO environment_deployment_runs(app_id, environment_id, app_release_id, config_revision_id, task_id, deployment_mode, plan_hash, plan_json, status, summary) VALUES (?1, ?2, ?3, ?4, ?5, 'normal', 'hash', ?6, 'partial_failed', '部分失败')",
        )
        .bind(app_id)
        .bind(environment_id)
        .bind(app_release_id)
        .bind(config_id)
        .bind(task_id)
        .bind(r#"{"units":[{"unit":"api"}]}"#)
        .execute(&db)
        .await
        .expect("insert run")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO deployment_unit_run_results(deployment_run_id, unit_id, unit_release_id, stage_no, action, status, failure_kind, failure_summary) VALUES (?1, ?2, ?3, 1, 'deploy', 'failed', 'health_failed', '健康检查失败')",
        )
        .bind(run_id)
        .bind(unit_id)
        .bind(artifact_id)
        .execute(&db)
        .await
        .expect("insert unit result");
        let service = DeploymentRetentionService::new(db.clone());

        let released = service
            .delete_deployment_snapshot(run_id, "operator")
            .await
            .expect("delete snapshot");

        assert!(released > 2);
        let run: (String, String, String) = sqlx::query_as(
            "SELECT snapshot_status, plan_hash, summary FROM environment_deployment_runs WHERE id = ?1",
        )
        .bind(run_id)
        .fetch_one(&db)
        .await
        .expect("load run");
        assert_eq!(
            run,
            ("deleted".to_owned(), String::new(), "部分失败".to_owned())
        );
        let result: (Option<i64>, String, String, String) = sqlx::query_as(
            "SELECT unit_release_id, status, failure_kind, failure_summary FROM deployment_unit_run_results WHERE deployment_run_id = ?1",
        )
        .bind(run_id)
        .fetch_one(&db)
        .await
        .expect("load structured result");
        assert_eq!(
            result,
            (
                None,
                "failed".to_owned(),
                "health_failed".to_owned(),
                "健康检查失败".to_owned()
            )
        );
        let cleanup_target: String = sqlx::query_scalar(
            "SELECT target_type FROM event_logs WHERE event_type = 'deployment.cleanup' AND target_id = ?1 ORDER BY id DESC LIMIT 1",
        )
        .bind(run_id.to_string())
        .fetch_one(&db)
        .await
        .expect("load snapshot cleanup event");
        assert_eq!(cleanup_target, "environment_deployment_run");
    }

    #[tokio::test]
    async fn deleting_deployment_history_releases_application_and_artifact_chain() {
        let db = retention_database().await;
        let app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('history-delete-app', 'History Delete', 'compose', 'compose', '/srv/app', 'ready')",
        )
        .execute(&db)
        .await
        .expect("insert app")
        .last_insert_rowid();
        let environment_id = sqlx::query(
            "INSERT INTO app_environments(app_id, environment_key, name, status) VALUES (?1, 'production', '正式环境', 'ready')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert environment")
        .last_insert_rowid();
        let unit_id = sqlx::query(
            "INSERT INTO deployment_units(app_id, unit_key, name) VALUES (?1, 'api', 'API')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert unit")
        .last_insert_rowid();
        let artifact_id = insert_artifact(&db, unit_id, "4.0.0", "/tmp/history-delete.tgz").await;
        let config_id = sqlx::query(
            "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, config_hash) VALUES (?1, 100, '{}', '{}', 'history-delete-config')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert config")
        .last_insert_rowid();
        let app_release_id = sqlx::query(
            "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '4.0.0', 100, 'received', 'openapi')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert app release")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO application_release_manifests(app_release_id, manifest_hash, manifest_json) VALUES (?1, 'history-delete-manifest', '{\"units\":[\"api\"]}')",
        )
        .bind(app_release_id)
        .execute(&db)
        .await
        .expect("insert manifest");
        sqlx::query(
            "INSERT INTO app_release_units(app_release_id, unit_id, unit_release_id) VALUES (?1, ?2, ?3)",
        )
        .bind(app_release_id)
        .bind(unit_id)
        .bind(artifact_id)
        .execute(&db)
        .await
        .expect("insert release unit");
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, app_id, release_id, environment_id, status) VALUES ('release.deploy', 'history delete', ?1, ?2, ?3, 'failed')",
        )
        .bind(app_id)
        .bind(app_release_id)
        .bind(environment_id)
        .execute(&db)
        .await
        .expect("insert task")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO operation_task_logs(task_id, stream, content) VALUES (?1, 'stdout', 'deploy failed')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert task log");
        sqlx::query(
            "INSERT INTO app_release_queue(app_id, release_id, status, triggered_by, task_id, environment_id) VALUES (?1, ?2, 'failed', 'operator', ?3, ?4)",
        )
        .bind(app_id)
        .bind(app_release_id)
        .bind(task_id)
        .bind(environment_id)
        .execute(&db)
        .await
        .expect("insert queue row");
        let run_id = sqlx::query(
            "INSERT INTO environment_deployment_runs(app_id, environment_id, app_release_id, config_revision_id, task_id, deployment_mode, plan_hash, plan_json, status, summary, finished_at) VALUES (?1, ?2, ?3, ?4, ?5, 'normal', 'hash', ?6, 'all_failed', '全部失败', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        )
        .bind(app_id)
        .bind(environment_id)
        .bind(app_release_id)
        .bind(config_id)
        .bind(task_id)
        .bind(r#"{"units":[{"unit":"api"}]}"#)
        .execute(&db)
        .await
        .expect("insert run")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO deployment_unit_run_results(deployment_run_id, unit_id, unit_release_id, stage_no, action, status, failure_kind, failure_summary) VALUES (?1, ?2, ?3, 1, 'deploy', 'failed', 'script_failed', '脚本失败')",
        )
        .bind(run_id)
        .bind(unit_id)
        .bind(artifact_id)
        .execute(&db)
        .await
        .expect("insert unit result");

        let retention = DeploymentRetentionService::new(db.clone());
        let blocked = retention
            .preview_deployment_history_delete(app_id, run_id)
            .await
            .expect("preview blocked history delete");
        assert!(!blocked.allowed());
        assert!(blocked.blockers.iter().any(|item| item.contains("快照")));
        assert!(blocked.blockers.iter().any(|item| item.contains("日志")));

        DeploymentLogService::new(db.clone())
            .delete_task_logs(task_id, "operator")
            .await
            .expect("delete logs first");
        retention
            .delete_deployment_snapshot(run_id, "operator")
            .await
            .expect("delete snapshot second");
        let ready = retention
            .preview_deployment_history_delete(app_id, run_id)
            .await
            .expect("preview ready history delete");
        assert!(ready.allowed());
        assert_eq!(ready.unit_result_count, 1);

        let deleted = retention
            .delete_deployment_history(app_id, run_id, "operator")
            .await
            .expect("delete deployment history");
        assert_eq!(deleted.deleted_unit_results, 1);
        assert_eq!(deleted.deleted_queue_rows, 1);
        assert!(deleted.cleared_task_release);

        let run_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM environment_deployment_runs WHERE id = ?1)",
        )
        .bind(run_id)
        .fetch_one(&db)
        .await
        .expect("check run deleted");
        assert!(!run_exists);
        let result_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deployment_unit_run_results WHERE deployment_run_id = ?1",
        )
        .bind(run_id)
        .fetch_one(&db)
        .await
        .expect("check unit results deleted");
        assert_eq!(result_count, 0);
        let task_release_id: Option<i64> =
            sqlx::query_scalar("SELECT release_id FROM operation_tasks WHERE id = ?1")
                .bind(task_id)
                .fetch_one(&db)
                .await
                .expect("load retained task");
        assert!(task_release_id.is_none());

        let archived = retention
            .archive_application_release(app_id, app_release_id, "operator")
            .await
            .expect("archive app release after history delete");
        assert!(archived.can_delete());
        retention
            .delete_application_release(app_id, app_release_id, "operator")
            .await
            .expect("delete app release after history delete");
        let artifact = retention
            .preview_artifact_cleanup(artifact_id)
            .await
            .expect("preview artifact after app release delete");
        assert!(artifact.allowed());

        let history_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM event_logs WHERE event_type = 'deployment.cleanup' AND target_type = 'environment_deployment_run' AND target_id = ?1 AND title = '部署历史已彻底删除'",
        )
        .bind(run_id.to_string())
        .fetch_one(&db)
        .await
        .expect("count history delete event");
        assert_eq!(history_event_count, 1);
    }

    #[tokio::test]
    async fn concrete_deleter_removes_only_local_paths_inside_data_directory() {
        let data_dir = tempfile::tempdir().expect("create data dir");
        let package_dir = data_dir.path().join("artifacts");
        let extract_dir = package_dir.join("release");
        std::fs::create_dir_all(&extract_dir).expect("create extract dir");
        let package_path = package_dir.join("release.tgz");
        std::fs::write(&package_path, b"package").expect("write package");
        std::fs::write(extract_dir.join("compose.yaml"), b"services: {}")
            .expect("write extracted file");
        let outside = tempfile::NamedTempFile::new().expect("create outside file");
        let deleter = ArtifactStorageDeleter::new(
            data_dir.path(),
            crate::artifact_storage::ArtifactStorageConfig::default(),
        );

        deleter
            .delete(&ArtifactDeletionTarget {
                provider: "local".to_owned(),
                package_path: package_path.to_string_lossy().into_owned(),
                extract_dir: extract_dir.to_string_lossy().into_owned(),
                bucket: String::new(),
                endpoint: String::new(),
                object_key: String::new(),
                object_version_id: String::new(),
            })
            .await
            .expect("delete local artifact");
        assert!(!package_path.exists());
        assert!(!extract_dir.exists());

        let error = deleter
            .delete(&ArtifactDeletionTarget {
                provider: "local".to_owned(),
                package_path: outside.path().to_string_lossy().into_owned(),
                extract_dir: String::new(),
                bucket: String::new(),
                endpoint: String::new(),
                object_key: String::new(),
                object_version_id: String::new(),
            })
            .await
            .expect_err("outside path must stay protected");
        assert!(error.contains("数据目录"));
        assert!(outside.path().exists());
    }

    #[tokio::test]
    async fn concrete_deleter_uses_stored_oss_bucket_endpoint_and_exact_version() {
        let data_dir = tempfile::tempdir().expect("create data dir");
        let verifier = Arc::new(RecordingOssVerifier::default());
        let deleter = ArtifactStorageDeleter::with_oss_verifier(
            data_dir.path(),
            crate::artifact_storage::ArtifactStorageConfig {
                provider: "local".to_owned(),
                aliyun_oss: crate::artifact_storage::AliyunOssConfig::default(),
            },
            verifier.clone(),
        );
        let mut target = ArtifactDeletionTarget {
            provider: "aliyun_oss".to_owned(),
            package_path: String::new(),
            extract_dir: String::new(),
            bucket: "release-bucket".to_owned(),
            endpoint: "https://oss-cn-shanghai.aliyuncs.com".to_owned(),
            object_key: "releases/api-1.0.0.tgz".to_owned(),
            object_version_id: "version-42".to_owned(),
        };

        deleter.delete(&target).await.expect("delete exact version");
        {
            let calls = verifier.calls.lock().expect("lock delete calls");
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, "release-bucket");
            assert_eq!(calls[0].1, "https://oss-cn-shanghai.aliyuncs.com");
            assert_eq!(calls[0].2, "releases/api-1.0.0.tgz");
            assert_eq!(calls[0].3.as_deref(), Some("version-42"));
        }

        target.object_version_id.clear();
        let error = deleter
            .delete(&target)
            .await
            .expect_err("missing exact version must be rejected");
        assert!(error.contains("精确对象版本号"));
        assert_eq!(verifier.calls.lock().expect("lock delete calls").len(), 1);
    }

    async fn retention_database() -> SqlitePool {
        let db = SqlitePool::connect_with(
            "sqlite::memory:"
                .parse::<SqliteConnectOptions>()
                .expect("sqlite options")
                .foreign_keys(true),
        )
        .await
        .expect("connect sqlite");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("run migrations");
        db
    }

    async fn insert_artifact(db: &SqlitePool, unit_id: i64, version: &str, path: &str) -> i64 {
        sqlx::query(
            "INSERT INTO deployment_unit_releases(unit_id, version, version_code, package_name, package_path, checksum_sha256, size_bytes) VALUES (?1, ?2, ?3, 'artifact.tgz', ?4, ?5, 42)",
        )
        .bind(unit_id)
        .bind(version)
        .bind(match version {
            "1.0.0" => 100,
            "1.1.0" => 101,
            _ => 102,
        })
        .bind(path)
        .bind("a".repeat(64))
        .execute(db)
        .await
        .expect("insert artifact")
        .last_insert_rowid()
    }

    #[derive(Default)]
    struct FailingOnceDeleter {
        failed: AtomicBool,
    }

    #[async_trait]
    impl ArtifactObjectDeleter for FailingOnceDeleter {
        async fn delete(&self, _target: &ArtifactDeletionTarget) -> Result<(), String> {
            if !self.failed.swap(true, Ordering::SeqCst) {
                Err("temporary object storage failure".to_owned())
            } else {
                Ok(())
            }
        }
    }

    type RecordedOssDelete = (String, String, String, Option<String>);

    #[derive(Default)]
    struct RecordingOssVerifier {
        calls: std::sync::Mutex<Vec<RecordedOssDelete>>,
    }

    #[async_trait]
    impl crate::artifact_storage::ArtifactObjectVerifier for RecordingOssVerifier {
        async fn verify(
            &self,
            _config: &crate::artifact_storage::AliyunOssConfig,
            _object_key: &str,
        ) -> Result<
            crate::artifact_storage::VerifiedArtifactObject,
            crate::artifact_storage::ArtifactStorageError,
        > {
            unreachable!("verification is not used by the cleanup deleter")
        }

        async fn delete(
            &self,
            config: &crate::artifact_storage::AliyunOssConfig,
            object_key: &str,
            version_id: Option<&str>,
        ) -> Result<(), crate::artifact_storage::ArtifactStorageError> {
            self.calls.lock().expect("lock delete calls").push((
                config.bucket.clone(),
                config.endpoint.clone(),
                object_key.to_owned(),
                version_id.map(str::to_owned),
            ));
            Ok(())
        }
    }
}
