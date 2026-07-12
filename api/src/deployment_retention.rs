use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use async_trait::async_trait;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::Mutex;

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
    pub bucket: String,
    pub object_key: String,
    pub object_version_id: String,
}

#[async_trait]
pub trait ArtifactObjectDeleter: Send + Sync {
    async fn delete(&self, target: &ArtifactDeletionTarget) -> Result<(), String>;
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

    pub async fn delete_task_logs(&self, task_id: i64) -> Result<u64, DeploymentRetentionError> {
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
        tx.commit().await?;
        self.state.lock().await.tasks.remove(&task_id);
        Ok((bounded_bytes + legacy_bytes).max(0) as u64)
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
        let target = sqlx::query_as::<_, (String, String, String, String, String)>(
            r#"
            SELECT storage_provider, package_path, storage_bucket,
                   storage_object_key, storage_object_version_id
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
            bucket: target.2,
            object_key: target.3,
            object_version_id: target.4,
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

async fn insert_cleanup_event(
    tx: &mut Transaction<'_, Sqlite>,
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
        ) VALUES ('deployment.cleanup', ?1, 'deployment_artifact', ?2, '', ?3, ?4, ?5)
        "#,
    )
    .bind(level)
    .bind(target_id.to_string())
    .bind(title)
    .bind(format!("操作人：{}", operator.trim()))
    .bind(detail)
    .execute(&mut **tx)
    .await?;
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
            .delete_task_logs(task_id)
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
            .delete_task_logs(task_id)
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
}
