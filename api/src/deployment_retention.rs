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
    tasks: HashMap<i64, TaskLogBudget>,
    steps: HashMap<i64, ActiveStepLog>,
}

struct ActiveStepLog {
    task_id: i64,
    buffer: BoundedLogBuffer,
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
        self.write(task_id, step_id, secrets, Some(chunk), false)
            .await
    }

    pub async fn finish(
        &self,
        task_id: i64,
        step_id: i64,
    ) -> Result<BoundedLogSnapshot, DeploymentRetentionError> {
        self.write(task_id, step_id, &[], None, true).await
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

    pub async fn delete_task_logs(&self, task_id: i64) -> Result<u64, DeploymentRetentionError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
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
        let mut state = self.state.lock().await;
        state.tasks.remove(&task_id);
        state.steps.retain(|_, step| step.task_id != task_id);
        Ok((bounded_bytes + legacy_bytes).max(0) as u64)
    }

    async fn write(
        &self,
        task_id: i64,
        step_id: i64,
        secrets: &[String],
        chunk: Option<&[u8]>,
        finish: bool,
    ) -> Result<BoundedLogSnapshot, DeploymentRetentionError> {
        let valid_step: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM operation_task_steps WHERE id = ?1 AND task_id = ?2)",
        )
        .bind(step_id)
        .bind(task_id)
        .fetch_one(&self.db)
        .await?;
        if !valid_step {
            return Err(DeploymentRetentionError::NotFound(
                "任务步骤不存在".to_owned(),
            ));
        }
        let mut state = self.state.lock().await;
        state
            .tasks
            .entry(task_id)
            .or_insert_with(|| TaskLogBudget::new(self.task_limit));
        state.steps.entry(step_id).or_insert_with(|| ActiveStepLog {
            task_id,
            buffer: BoundedLogBuffer::new(self.head_limit, self.tail_limit, secrets.to_vec()),
            finished: false,
        });
        let mut task_budget = state
            .tasks
            .remove(&task_id)
            .expect("task budget was inserted above");
        let step = state
            .steps
            .get_mut(&step_id)
            .expect("step buffer was inserted above");
        if step.task_id != task_id || step.finished {
            state.tasks.insert(task_id, task_budget);
            return Err(DeploymentRetentionError::InvalidState(
                "步骤日志已经结束或不属于该任务".to_owned(),
            ));
        }
        if let Some(chunk) = chunk {
            step.buffer.append(chunk, &mut task_budget);
        }
        if finish {
            step.buffer.finish(&mut task_budget);
            step.finished = true;
        }
        let snapshot = step.buffer.snapshot();
        let finished = step.finished;
        let task_values = (
            task_budget.stored_bytes(),
            task_budget.received_bytes(),
            task_budget.dropped_bytes(),
            task_budget.truncated(),
        );
        state.tasks.insert(task_id, task_budget);
        let mut tx = self.db.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO deployment_task_log_budgets(
                task_id, stored_bytes, received_bytes, dropped_bytes, max_bytes, truncated
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(task_id) DO UPDATE SET
                stored_bytes = excluded.stored_bytes,
                received_bytes = excluded.received_bytes,
                dropped_bytes = excluded.dropped_bytes,
                max_bytes = excluded.max_bytes,
                truncated = excluded.truncated,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(task_id)
        .bind(task_values.0 as i64)
        .bind(task_values.1 as i64)
        .bind(task_values.2 as i64)
        .bind(self.task_limit as i64)
        .bind(task_values.3)
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
                finished = excluded.finished,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(step_id)
        .bind(task_id)
        .bind(&snapshot.head)
        .bind(&snapshot.tail)
        .bind(snapshot.stored_bytes as i64)
        .bind(snapshot.received_bytes as i64)
        .bind(snapshot.dropped_bytes as i64)
        .bind(self.head_limit as i64)
        .bind(self.tail_limit as i64)
        .bind(snapshot.truncated)
        .bind(finished)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(snapshot)
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

    pub fn append(&mut self, chunk: &[u8], task_budget: &mut TaskLogBudget) {
        let redacted = self.redactor.push(chunk);
        self.append_redacted(&redacted, task_budget);
    }

    pub fn finish(&mut self, task_budget: &mut TaskLogBudget) {
        let redacted = self.redactor.finish();
        self.append_redacted(&redacted, task_budget);
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
        for byte in &bytes[offset..] {
            if self.tail.len() < self.tail_limit {
                if task_budget.allocate(1) == 1 {
                    self.tail.push_back(*byte);
                } else {
                    self.record_drop(1, task_budget);
                }
            } else if self.tail_limit > 0 {
                self.tail.pop_front();
                self.tail.push_back(*byte);
                self.record_drop(1, task_budget);
            } else {
                self.record_drop(1, task_budget);
            }
        }
    }

    fn record_drop(&mut self, count: u64, task_budget: &mut TaskLogBudget) {
        self.dropped_bytes += count;
        task_budget.dropped_bytes += count;
    }
}

#[derive(Debug)]
struct StreamingRedactor {
    secrets: Vec<Vec<u8>>,
    pending: Vec<u8>,
    overlap: usize,
    masking_assignment: bool,
}

impl StreamingRedactor {
    fn new(secrets: Vec<String>) -> Self {
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

    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        self.process(false)
    }

    fn finish(&mut self) -> Vec<u8> {
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
