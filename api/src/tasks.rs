use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::deploy::ComposeCommandOutput;

#[derive(Clone)]
pub struct TaskService {
    db: SqlitePool,
}

#[derive(Debug)]
pub enum TaskError {
    NotFound(String),
    InvalidState(String),
    Internal(String),
}

impl TaskError {
    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(message) | Self::InvalidState(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for TaskError {}

impl From<sqlx::Error> for TaskError {
    fn from(value: sqlx::Error) -> Self {
        if let sqlx::Error::Database(err) = &value
            && (err.is_unique_violation()
                || err
                    .message()
                    .contains("active deployment task exists for app"))
        {
            return Self::InvalidState("该应用已有等待中或执行中的部署任务".to_owned());
        }
        Self::Internal(format!("任务数据操作失败: {value}"))
    }
}

#[derive(Clone, Debug)]
pub struct CreateTaskInput {
    pub task_kind: String,
    pub title: String,
    pub app_id: Option<i64>,
    pub release_id: Option<i64>,
    pub node_id: Option<i64>,
    pub created_by: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskListItem {
    pub id: i64,
    pub task_kind: String,
    pub title: String,
    pub app_name: Option<String>,
    pub status: String,
    pub phase: String,
    pub command: String,
    pub summary: String,
    pub exit_code: Option<i64>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ActiveAppTaskItem {
    pub id: i64,
    pub task_kind: String,
    pub title: String,
    pub status: String,
    pub phase: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskQueuePositionItem {
    pub queued_before: i64,
    pub running_before: i64,
}

#[derive(Clone, Debug, Default)]
pub struct TaskListFilter {
    pub status: Option<String>,
    pub phase: Option<String>,
    pub app_id: Option<i64>,
    pub task_kind: Option<String>,
    pub query: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskStatusCount {
    pub status: String,
    pub count: i64,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskDetailItem {
    pub id: i64,
    pub app_id: Option<i64>,
    pub node_id: Option<i64>,
    pub task_kind: String,
    pub title: String,
    pub app_name: Option<String>,
    pub node_name: Option<String>,
    pub status: String,
    pub phase: String,
    pub command: String,
    pub summary: String,
    pub exit_code: Option<i64>,
    pub created_by: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskLogItem {
    pub id: i64,
    pub step_id: Option<i64>,
    pub stream: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskPhaseItem {
    pub id: i64,
    pub task_id: i64,
    pub phase_no: i64,
    pub phase_key: String,
    pub title: String,
    pub status: String,
    pub summary: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskStepItem {
    pub id: i64,
    pub task_id: i64,
    pub phase_id: Option<i64>,
    pub node_id: Option<i64>,
    pub node_name: Option<String>,
    pub step_no: i64,
    pub step_key: String,
    pub title: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct StartTaskPhaseInput<'a> {
    pub task_id: i64,
    pub phase_key: &'a str,
    pub title: &'a str,
}

#[derive(Clone, Debug)]
pub struct StartTaskStepInput<'a> {
    pub task_id: i64,
    pub node_id: Option<i64>,
    pub step_key: &'a str,
    pub title: &'a str,
    pub command: &'a str,
}

#[derive(Clone, Debug)]
pub struct TaskNodeResultInput<'a> {
    pub task_id: i64,
    pub node_id: i64,
    pub node_name: &'a str,
    pub node_key: &'a str,
    pub node_type: &'a str,
    pub status: &'a str,
    pub message: &'a str,
    pub command_count: i64,
}

#[derive(Clone, Debug)]
pub struct RecordDeploymentRunInput<'a> {
    pub app_id: i64,
    pub task_id: i64,
    pub release_id: Option<i64>,
    pub deploy_action: &'a str,
    pub status: &'a str,
    pub message: &'a str,
    pub config_snapshot_id: Option<i64>,
    pub config_revision_no: i64,
    pub artifact_version: &'a str,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskNodeResultItem {
    pub id: i64,
    pub task_id: i64,
    pub node_id: i64,
    pub node_name: String,
    pub node_key: String,
    pub node_type: String,
    pub status: String,
    pub message: String,
    pub command_count: i64,
    pub started_at: Option<String>,
    pub finished_at: String,
}

impl TaskService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskListItem>, TaskError> {
        self.list_tasks_filtered(TaskListFilter::default()).await
    }

    pub async fn list_tasks_filtered(
        &self,
        filter: TaskListFilter,
    ) -> Result<Vec<TaskListItem>, TaskError> {
        let filter = normalize_task_filter(filter)?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
            SELECT
                t.id,
                t.task_kind,
                t.title,
                a.name AS app_name,
                t.status,
                t.phase,
                t.command,
                t.summary,
                t.exit_code,
                t.created_by,
                t.created_at,
                t.updated_at
            FROM operation_tasks t
            LEFT JOIN apps a ON a.id = t.app_id
            WHERE 1 = 1
            "#,
        );
        push_task_filter_clauses(&mut builder, &filter, true);
        builder.push(
            r#"
            ORDER BY t.id DESC
            LIMIT 100
            "#,
        );
        builder
            .build_query_as::<TaskListItem>()
            .fetch_all(&self.db)
            .await
            .map_err(TaskError::from)
    }

    pub async fn task_status_counts(
        &self,
        filter: TaskListFilter,
    ) -> Result<Vec<TaskStatusCount>, TaskError> {
        let filter = normalize_task_filter(filter)?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
            SELECT t.status, COUNT(*) AS count
            FROM operation_tasks t
            LEFT JOIN apps a ON a.id = t.app_id
            WHERE 1 = 1
            "#,
        );
        push_task_filter_clauses(&mut builder, &filter, false);
        builder.push(
            r#"
            GROUP BY t.status
            "#,
        );
        builder
            .build_query_as::<TaskStatusCount>()
            .fetch_all(&self.db)
            .await
            .map_err(TaskError::from)
    }

    pub async fn task_detail(&self, task_id: i64) -> Result<TaskDetailItem, TaskError> {
        let task = sqlx::query_as::<_, TaskDetailItem>(
            r#"
            SELECT
                t.id,
                t.app_id,
                t.node_id,
                t.task_kind,
                t.title,
                a.name AS app_name,
                n.name AS node_name,
                t.status,
                t.phase,
                t.command,
                t.summary,
                t.exit_code,
                t.created_by,
                t.started_at,
                t.finished_at,
                t.created_at,
                t.updated_at
            FROM operation_tasks t
            LEFT JOIN apps a ON a.id = t.app_id
            LEFT JOIN nodes n ON n.id = t.node_id
            WHERE t.id = ?1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.db)
        .await?;
        task.ok_or_else(|| TaskError::NotFound("任务不存在".to_owned()))
    }

    pub async fn task_logs(&self, task_id: i64) -> Result<Vec<TaskLogItem>, TaskError> {
        sqlx::query_as::<_, TaskLogItem>(
            r#"
            SELECT id, step_id, stream, content, created_at
            FROM operation_task_logs
            WHERE task_id = ?1
            ORDER BY id ASC
            LIMIT 1000
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.db)
        .await
        .map_err(TaskError::from)
    }

    pub async fn task_phases(&self, task_id: i64) -> Result<Vec<TaskPhaseItem>, TaskError> {
        sqlx::query_as::<_, TaskPhaseItem>(
            r#"
            SELECT
                id,
                task_id,
                phase_no,
                phase_key,
                title,
                status,
                summary,
                started_at,
                finished_at,
                created_at,
                updated_at
            FROM operation_task_phases
            WHERE task_id = ?1
            ORDER BY phase_no ASC, id ASC
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.db)
        .await
        .map_err(TaskError::from)
    }

    pub async fn task_steps(&self, task_id: i64) -> Result<Vec<TaskStepItem>, TaskError> {
        sqlx::query_as::<_, TaskStepItem>(
            r#"
            SELECT
                s.id,
                s.task_id,
                s.phase_id,
                s.node_id,
                n.name AS node_name,
                s.step_no,
                s.step_key,
                s.title,
                s.command,
                s.status,
                s.exit_code,
                s.started_at,
                s.finished_at,
                s.created_at,
                s.updated_at
            FROM operation_task_steps s
            LEFT JOIN nodes n ON n.id = s.node_id
            WHERE s.task_id = ?1
            ORDER BY s.step_no ASC, s.id ASC
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.db)
        .await
        .map_err(TaskError::from)
    }

    pub async fn task_node_results(
        &self,
        task_id: i64,
    ) -> Result<Vec<TaskNodeResultItem>, TaskError> {
        sqlx::query_as::<_, TaskNodeResultItem>(
            r#"
            SELECT
                id,
                task_id,
                node_id,
                node_name,
                node_key,
                node_type,
                status,
                message,
                command_count,
                started_at,
                finished_at
            FROM operation_task_node_results
            WHERE task_id = ?1
            ORDER BY id ASC
            "#,
        )
        .bind(task_id)
        .fetch_all(&self.db)
        .await
        .map_err(TaskError::from)
    }

    pub async fn active_app_task(
        &self,
        app_id: i64,
    ) -> Result<Option<ActiveAppTaskItem>, TaskError> {
        sqlx::query_as::<_, ActiveAppTaskItem>(
            r#"
            SELECT id, task_kind, title, status, phase
            FROM operation_tasks
            WHERE app_id = ?1
              AND status IN ('queued', 'running')
              AND task_kind IN (
                'compose.up',
                'compose.down',
                'compose.restart',
                'binary.restart',
                'binary.stop',
                'release.deploy',
                'release.rollback',
                'release.manual_apply',
                'node.install.docker',
                'node.install.compose',
                'node.install.caddy',
                'node.install.nginx'
              )
            ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END, id ASC
            LIMIT 1
            "#,
        )
        .bind(app_id)
        .fetch_optional(&self.db)
        .await
        .map_err(TaskError::from)
    }

    pub async fn task_queue_position(
        &self,
        task_id: i64,
    ) -> Result<TaskQueuePositionItem, TaskError> {
        sqlx::query_as::<_, TaskQueuePositionItem>(
            r#"
            SELECT
                COALESCE(SUM(CASE
                    WHEN other.status = 'queued'
                     AND other.id < current.id
                    THEN 1 ELSE 0 END), 0) AS queued_before,
                COALESCE(SUM(CASE
                    WHEN other.status = 'running'
                    THEN 1 ELSE 0 END), 0) AS running_before
            FROM operation_tasks current
            LEFT JOIN operation_tasks other
              ON other.status IN ('queued', 'running')
             AND other.task_kind IN (
                'compose.up',
                'compose.down',
                'compose.restart',
                'binary.restart',
                'binary.stop',
                'release.deploy',
                'release.rollback',
                'release.manual_apply',
                'node.install.docker',
                'node.install.compose',
                'node.install.caddy',
                'node.install.nginx'
             )
             AND other.id != current.id
            WHERE current.id = ?1
            GROUP BY current.id
            "#,
        )
        .bind(task_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| TaskError::NotFound("任务不存在".to_owned()))
    }

    pub async fn create_task(&self, input: CreateTaskInput) -> Result<i64, TaskError> {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO operation_tasks(
                task_kind,
                title,
                app_id,
                release_id,
                node_id,
                status,
                created_by
            )
            VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)
            RETURNING id
            "#,
        )
        .bind(input.task_kind)
        .bind(input.title)
        .bind(input.app_id)
        .bind(input.release_id)
        .bind(input.node_id)
        .bind(input.created_by)
        .fetch_one(&self.db)
        .await
        .map_err(TaskError::from)
    }

    pub async fn mark_running(
        &self,
        task_id: i64,
        command: &str,
        phase: &str,
    ) -> Result<bool, TaskError> {
        let phase = normalize_task_phase(phase)?;
        let result = sqlx::query(
            r#"
            UPDATE operation_tasks
            SET status = 'running',
                phase = ?2,
                command = ?3,
                started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1 AND status = 'queued'
            "#,
        )
        .bind(task_id)
        .bind(phase)
        .bind(command)
        .execute(&self.db)
        .await?;
        if result.rows_affected() == 0 {
            let status = self.raw_status(task_id).await?;
            if status == "canceled" {
                return Ok(false);
            }
            return Err(TaskError::InvalidState(format!(
                "任务当前状态为 {status}，不能进入执行中"
            )));
        }
        self.append_log(task_id, "system", &format!("开始执行: {command}"))
            .await?;
        Ok(true)
    }

    pub async fn update_phase(&self, task_id: i64, phase: &str) -> Result<(), TaskError> {
        let phase = normalize_task_phase(phase)?;
        let phase_title = task_phase_title(phase);
        let mut tx = self.db.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE operation_tasks
            SET phase = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
              AND status = 'running'
            "#,
        )
        .bind(task_id)
        .bind(phase)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query(
            r#"
            UPDATE operation_task_phases
            SET status = 'success',
                finished_at = COALESCE(finished_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE task_id = ?1
              AND phase_key != ?2
              AND status = 'running'
            "#,
        )
        .bind(task_id)
        .bind(phase)
        .execute(&mut *tx)
        .await?;
        let updated_phase = sqlx::query(
            r#"
            UPDATE operation_task_phases
            SET title = ?3,
                status = 'running',
                started_at = COALESCE(started_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                finished_at = NULL,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE task_id = ?1
              AND phase_key = ?2
            "#,
        )
        .bind(task_id)
        .bind(phase)
        .bind(phase_title)
        .execute(&mut *tx)
        .await?;
        if updated_phase.rows_affected() == 0 {
            let phase_no = sqlx::query_scalar::<_, i64>(
                r#"
                SELECT COALESCE(MAX(phase_no), 0) + 1
                FROM operation_task_phases
                WHERE task_id = ?1
                "#,
            )
            .bind(task_id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                INSERT INTO operation_task_phases(
                    task_id,
                    phase_no,
                    phase_key,
                    title,
                    status,
                    started_at
                )
                VALUES (?1, ?2, ?3, ?4, 'running', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                "#,
            )
            .bind(task_id)
            .bind(phase_no)
            .bind(phase)
            .bind(phase_title)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn cancel_queued(&self, task_id: i64, actor: &str) -> Result<(), TaskError> {
        let message = format!("{actor} 取消了排队任务");
        let result = sqlx::query(
            r#"
            UPDATE operation_tasks
            SET status = 'canceled',
                phase = 'canceled',
                summary = ?2,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1 AND status = 'queued'
            "#,
        )
        .bind(task_id)
        .bind(&message)
        .execute(&self.db)
        .await?;
        if result.rows_affected() == 0 {
            let status = self.raw_status(task_id).await?;
            return Err(TaskError::InvalidState(format!(
                "任务当前状态为 {status}，只能取消等待中的任务"
            )));
        }
        self.append_log(task_id, "system", &message).await
    }

    pub async fn finish_with_compose_output(
        &self,
        task_id: i64,
        output: &ComposeCommandOutput,
    ) -> Result<(), TaskError> {
        let status = if output.success { "success" } else { "failed" };
        let summary = if output.output.trim().is_empty() {
            "命令没有输出".to_owned()
        } else {
            first_lines(&output.output, 3)
        };
        sqlx::query(
            r#"
            UPDATE operation_tasks
            SET status = ?2,
                phase = ?3,
                command = ?4,
                summary = ?5,
                exit_code = ?6,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(task_id)
        .bind(status)
        .bind(if output.success {
            "completed"
        } else {
            "failed"
        })
        .bind(&output.command)
        .bind(summary)
        .bind(output.status_code)
        .execute(&self.db)
        .await?;
        self.finish_open_phases(task_id, if output.success { "success" } else { "failed" })
            .await?;
        self.append_log(task_id, "combined", &output.output).await
    }

    pub async fn finish_success(
        &self,
        task_id: i64,
        command: &str,
        summary: &str,
    ) -> Result<(), TaskError> {
        sqlx::query(
            r#"
            UPDATE operation_tasks
            SET status = 'success',
                phase = 'completed',
                command = ?2,
                summary = ?3,
                exit_code = 0,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(task_id)
        .bind(command)
        .bind(summary)
        .execute(&self.db)
        .await?;
        self.finish_open_phases(task_id, "success").await?;
        self.append_log(task_id, "system", summary).await
    }

    pub async fn record_node_result(
        &self,
        input: TaskNodeResultInput<'_>,
    ) -> Result<(), TaskError> {
        let status = normalize_node_result_status(input.status)?;
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
                command_count,
                started_at
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
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            )
            ON CONFLICT(task_id, node_id) DO UPDATE SET
                node_name = excluded.node_name,
                node_key = excluded.node_key,
                node_type = excluded.node_type,
                status = excluded.status,
                message = excluded.message,
                command_count = excluded.command_count,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(input.task_id)
        .bind(input.node_id)
        .bind(input.node_name)
        .bind(input.node_key)
        .bind(input.node_type)
        .bind(status)
        .bind(input.message)
        .bind(input.command_count)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn start_phase(&self, input: StartTaskPhaseInput<'_>) -> Result<i64, TaskError> {
        let phase_key = normalize_step_key(input.phase_key)?;
        let title = required_step_text(input.title, "任务阶段名称不能为空")?;
        let mut tx = self.db.begin().await?;
        let phase_no = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COALESCE(MAX(phase_no), 0) + 1
            FROM operation_task_phases
            WHERE task_id = ?1
            "#,
        )
        .bind(input.task_id)
        .fetch_one(&mut *tx)
        .await?;
        let phase_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO operation_task_phases(
                task_id,
                phase_no,
                phase_key,
                title,
                status,
                started_at
            )
            VALUES (?1, ?2, ?3, ?4, 'running', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            RETURNING id
            "#,
        )
        .bind(input.task_id)
        .bind(phase_no)
        .bind(phase_key)
        .bind(title)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO operation_task_logs(task_id, stream, content)
            VALUES (?1, 'system', ?2)
            "#,
        )
        .bind(input.task_id)
        .bind(format!("开始阶段: {}", title))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(phase_id)
    }

    pub async fn finish_phase(
        &self,
        task_id: i64,
        phase_id: i64,
        status: &str,
        summary: &str,
    ) -> Result<(), TaskError> {
        let status = normalize_step_status(status)?;
        let summary = summary.trim();
        let mut tx = self.db.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE operation_task_phases
            SET status = ?3,
                summary = ?4,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE task_id = ?1
              AND id = ?2
            "#,
        )
        .bind(task_id)
        .bind(phase_id)
        .bind(status)
        .bind(summary)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(TaskError::NotFound("任务阶段不存在".to_owned()));
        }
        if !summary.is_empty() {
            sqlx::query(
                r#"
                INSERT INTO operation_task_logs(task_id, stream, content)
                VALUES (?1, 'system', ?2)
                "#,
            )
            .bind(task_id)
            .bind(summary)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn start_step(&self, input: StartTaskStepInput<'_>) -> Result<i64, TaskError> {
        let step_key = normalize_step_key(input.step_key)?;
        let title = required_step_text(input.title, "步骤名称不能为空")?;
        let command = input.command.trim();
        let mut tx = self.db.begin().await?;
        let step_no = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COALESCE(MAX(step_no), 0) + 1
            FROM operation_task_steps
            WHERE task_id = ?1
            "#,
        )
        .bind(input.task_id)
        .fetch_one(&mut *tx)
        .await?;
        let phase_id = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT id
            FROM operation_task_phases
            WHERE task_id = ?1
              AND status = 'running'
            ORDER BY phase_no DESC, id DESC
            LIMIT 1
            "#,
        )
        .bind(input.task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let step_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO operation_task_steps(
                task_id,
                phase_id,
                node_id,
                step_no,
                step_key,
                title,
                command,
                status,
                started_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            RETURNING id
            "#,
        )
        .bind(input.task_id)
        .bind(phase_id)
        .bind(input.node_id)
        .bind(step_no)
        .bind(step_key)
        .bind(title)
        .bind(command)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            INSERT INTO operation_task_logs(task_id, step_id, stream, content)
            VALUES (?1, ?2, 'system', ?3)
            "#,
        )
        .bind(input.task_id)
        .bind(step_id)
        .bind(format!("开始步骤: {}", title))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(step_id)
    }

    pub async fn append_step_log(
        &self,
        task_id: i64,
        step_id: i64,
        stream: &str,
        content: &str,
    ) -> Result<(), TaskError> {
        let stream = normalize_log_stream(stream)?;
        sqlx::query(
            r#"
            INSERT INTO operation_task_logs(task_id, step_id, stream, content)
            VALUES (?1, ?2, ?3, ?4)
            "#,
        )
        .bind(task_id)
        .bind(step_id)
        .bind(stream)
        .bind(content)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn finish_step(
        &self,
        task_id: i64,
        step_id: i64,
        exit_code: Option<i64>,
        summary: &str,
    ) -> Result<(), TaskError> {
        self.finish_step_with_status(task_id, step_id, "success", exit_code, summary)
            .await
    }

    pub async fn fail_step(
        &self,
        task_id: i64,
        step_id: i64,
        exit_code: Option<i64>,
        summary: &str,
    ) -> Result<(), TaskError> {
        self.finish_step_with_status(task_id, step_id, "failed", exit_code, summary)
            .await
    }

    pub async fn skip_step(
        &self,
        task_id: i64,
        step_id: i64,
        summary: &str,
    ) -> Result<(), TaskError> {
        self.finish_step_with_status(task_id, step_id, "skipped", None, summary)
            .await
    }

    async fn finish_step_with_status(
        &self,
        task_id: i64,
        step_id: i64,
        status: &str,
        exit_code: Option<i64>,
        summary: &str,
    ) -> Result<(), TaskError> {
        let status = normalize_step_status(status)?;
        let mut tx = self.db.begin().await?;
        let result = sqlx::query(
            r#"
            UPDATE operation_task_steps
            SET status = ?3,
                exit_code = ?4,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE task_id = ?1
              AND id = ?2
            "#,
        )
        .bind(task_id)
        .bind(step_id)
        .bind(status)
        .bind(exit_code)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(TaskError::NotFound("任务步骤不存在".to_owned()));
        }
        let summary = summary.trim();
        if !summary.is_empty() {
            sqlx::query(
                r#"
                INSERT INTO operation_task_logs(task_id, step_id, stream, content)
                VALUES (?1, ?2, 'system', ?3)
                "#,
            )
            .bind(task_id)
            .bind(step_id)
            .bind(summary)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn fail_task(&self, task_id: i64, message: &str) -> Result<(), TaskError> {
        sqlx::query(
            r#"
            UPDATE operation_tasks
            SET status = 'failed',
                phase = 'failed',
                summary = ?2,
                finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(task_id)
        .bind(message)
        .execute(&self.db)
        .await?;
        self.finish_open_phases(task_id, "failed").await?;
        self.append_log(task_id, "system", message).await
    }

    async fn finish_open_phases(&self, task_id: i64, status: &str) -> Result<(), TaskError> {
        let status = normalize_step_status(status)?;
        sqlx::query(
            r#"
            UPDATE operation_task_phases
            SET status = ?2,
                finished_at = COALESCE(finished_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE task_id = ?1
              AND status = 'running'
            "#,
        )
        .bind(task_id)
        .bind(status)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn finish_failed(&self, task_id: i64, message: &str) -> Result<(), TaskError> {
        self.fail_task(task_id, message).await
    }

    pub async fn append_log(
        &self,
        task_id: i64,
        stream: &str,
        content: &str,
    ) -> Result<(), TaskError> {
        let stream = normalize_log_stream(stream)?;
        sqlx::query(
            r#"
            INSERT INTO operation_task_logs(task_id, stream, content)
            VALUES (?1, ?2, ?3)
            "#,
        )
        .bind(task_id)
        .bind(stream)
        .bind(content)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn record_deployment_run(
        &self,
        input: RecordDeploymentRunInput<'_>,
    ) -> Result<(), TaskError> {
        sqlx::query(
            r#"
            INSERT INTO deployment_runs(
                app_id,
                task_id,
                release_id,
                deploy_action,
                status,
                finished_at,
                message,
                config_snapshot_id,
                config_revision_no,
                artifact_version
            )
            VALUES (
                ?1,
                ?2,
                ?3,
                ?4,
                ?5,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                ?6,
                ?7,
                ?8,
                ?9
            )
            "#,
        )
        .bind(input.app_id)
        .bind(input.task_id)
        .bind(input.release_id)
        .bind(input.deploy_action)
        .bind(input.status)
        .bind(input.message)
        .bind(input.config_snapshot_id)
        .bind(input.config_revision_no)
        .bind(input.artifact_version)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn raw_status(&self, task_id: i64) -> Result<String, TaskError> {
        sqlx::query_scalar::<_, String>("SELECT status FROM operation_tasks WHERE id = ?1")
            .bind(task_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| TaskError::NotFound("任务不存在".to_owned()))
    }
}

pub fn active_task_status_label(status: &str) -> &'static str {
    match status {
        "queued" => "等待中",
        "running" => "执行中",
        _ => "活跃",
    }
}

fn first_lines(value: &str, limit: usize) -> String {
    let lines = value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(limit)
        .collect::<Vec<_>>()
        .join("\n");
    if lines.is_empty() {
        "命令没有输出".to_owned()
    } else {
        lines
    }
}

fn normalize_task_filter(mut filter: TaskListFilter) -> Result<TaskListFilter, TaskError> {
    filter.status = normalize_optional_filter(filter.status);
    if let Some(status) = &filter.status
        && !matches!(
            status.as_str(),
            "queued" | "running" | "success" | "failed" | "canceled"
        )
    {
        return Err(TaskError::InvalidState("任务状态筛选不支持".to_owned()));
    }
    filter.phase = normalize_optional_filter(filter.phase);
    if let Some(phase) = &filter.phase {
        normalize_task_phase(phase)?;
    }
    filter.task_kind = normalize_optional_filter(filter.task_kind);
    if let Some(task_kind) = &filter.task_kind
        && !matches!(
            task_kind.as_str(),
            "compose.up"
                | "compose.down"
                | "compose.restart"
                | "binary.restart"
                | "binary.stop"
                | "release.deploy"
                | "release.rollback"
                | "release.manual_apply"
                | "node.install.docker"
                | "node.install.compose"
                | "node.install.caddy"
                | "node.install.nginx"
        )
    {
        return Err(TaskError::InvalidState("任务类型筛选不支持".to_owned()));
    }
    filter.query =
        normalize_optional_filter(filter.query).map(|query| query.chars().take(80).collect());
    Ok(filter)
}

fn normalize_task_phase(phase: &str) -> Result<&str, TaskError> {
    match phase {
        "queued" | "preflight" | "preparing_files" | "executing" | "healthchecking" | "prepare"
        | "render" | "pre_deploy" | "deploy" | "post_deploy" | "switch_traffic" | "cleanup"
        | "finalize" | "completed" | "failed" | "canceled" => Ok(phase),
        _ => Err(TaskError::InvalidState("任务阶段不支持".to_owned())),
    }
}

fn task_phase_title(phase: &str) -> &'static str {
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

fn normalize_node_result_status(status: &str) -> Result<&str, TaskError> {
    match status {
        "success" | "failed" | "skipped" => Ok(status),
        _ => Err(TaskError::InvalidState(format!(
            "节点任务结果状态无效: {status}"
        ))),
    }
}

fn normalize_step_status(status: &str) -> Result<&str, TaskError> {
    match status {
        "pending" | "running" | "success" | "failed" | "skipped" => Ok(status),
        _ => Err(TaskError::InvalidState(format!(
            "任务步骤状态无效: {status}"
        ))),
    }
}

fn normalize_log_stream(stream: &str) -> Result<&str, TaskError> {
    match stream {
        "system" | "stdout" | "stderr" | "combined" => Ok(stream),
        _ => Err(TaskError::InvalidState(format!("任务日志流无效: {stream}"))),
    }
}

fn normalize_step_key(value: &str) -> Result<&str, TaskError> {
    let value = value.trim();
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(TaskError::InvalidState(
            "任务步骤标识仅支持字母、数字、点、短横线和下划线".to_owned(),
        ));
    }
    Ok(value)
}

fn required_step_text<'a>(value: &'a str, message: &str) -> Result<&'a str, TaskError> {
    let value = value.trim();
    if value.is_empty() {
        Err(TaskError::InvalidState(message.to_owned()))
    } else {
        Ok(value)
    }
}

fn normalize_optional_filter(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn push_task_filter_clauses(
    builder: &mut QueryBuilder<'_, Sqlite>,
    filter: &TaskListFilter,
    include_status: bool,
) {
    if include_status && let Some(status) = &filter.status {
        builder.push(" AND t.status = ");
        builder.push_bind(status.clone());
    }
    if let Some(phase) = &filter.phase {
        builder.push(" AND t.phase = ");
        builder.push_bind(phase.clone());
    }
    if let Some(app_id) = filter.app_id {
        builder.push(" AND t.app_id = ");
        builder.push_bind(app_id);
    }
    if let Some(task_kind) = &filter.task_kind {
        builder.push(" AND t.task_kind = ");
        builder.push_bind(task_kind.clone());
    }
    if let Some(query) = &filter.query {
        let like_query = format!("%{query}%");
        builder.push(" AND (t.title LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR t.command LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR t.summary LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR a.name LIKE ");
        builder.push_bind(like_query);
        builder.push(")");
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqliteConnectOptions;

    use super::*;

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

    #[tokio::test]
    async fn task_node_results_are_recorded_and_replaced_per_node() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "Deploy multi node app".to_owned(),
                app_id: None,
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");

        tasks
            .record_node_result(TaskNodeResultInput {
                task_id,
                node_id: 1,
                node_name: "Local node",
                node_key: "local",
                node_type: "local",
                status: "failed",
                message: "preflight failed",
                command_count: 2,
            })
            .await
            .expect("record first node result");
        tasks
            .record_node_result(TaskNodeResultInput {
                task_id,
                node_id: 1,
                node_name: "Local node",
                node_key: "local",
                node_type: "local",
                status: "success",
                message: "retry succeeded",
                command_count: 4,
            })
            .await
            .expect("replace node result");

        let results = tasks
            .task_node_results(task_id)
            .await
            .expect("task node results");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node_key, "local");
        assert_eq!(results[0].status, "success");
        assert_eq!(results[0].message, "retry succeeded");
        assert_eq!(results[0].command_count, 4);
    }

    #[tokio::test]
    async fn task_steps_record_status_and_link_logs() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "Deploy redis".to_owned(),
                app_id: None,
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");

        let step_id = tasks
            .start_step(StartTaskStepInput {
                task_id,
                node_id: None,
                step_key: "compose.config",
                title: "校验 Compose 配置",
                command: "docker compose config",
            })
            .await
            .expect("start step");
        tasks
            .append_step_log(task_id, step_id, "combined", "services:\n  redis:\n")
            .await
            .expect("append step log");
        tasks
            .finish_step(task_id, step_id, Some(0), "Compose 配置校验通过")
            .await
            .expect("finish step");

        let steps = tasks.task_steps(task_id).await.expect("task steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, step_id);
        assert_eq!(steps[0].step_no, 1);
        assert_eq!(steps[0].step_key, "compose.config");
        assert_eq!(steps[0].status, "success");
        assert_eq!(steps[0].exit_code, Some(0));

        let logs = tasks.task_logs(task_id).await.expect("task logs");
        assert!(logs.iter().any(|log| log.step_id == Some(step_id)
            && log.stream == "combined"
            && log.content.contains("redis")));
        assert!(logs.iter().any(|log| log.step_id == Some(step_id)
            && log.stream == "system"
            && log.content.contains("Compose 配置校验通过")));
    }

    #[tokio::test]
    async fn task_steps_attach_to_current_phase() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "release.deploy".to_owned(),
                title: "Deploy orders".to_owned(),
                app_id: None,
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");

        assert!(
            tasks
                .mark_running(task_id, "release deploy", "preflight")
                .await
                .expect("mark running")
        );
        tasks
            .update_phase(task_id, "preflight")
            .await
            .expect("start preflight phase");
        let preflight_step_id = tasks
            .start_step(StartTaskStepInput {
                task_id,
                node_id: None,
                step_key: "node.preflight",
                title: "Node preflight",
                command: "docker info",
            })
            .await
            .expect("start preflight step");
        tasks
            .finish_step(task_id, preflight_step_id, Some(0), "preflight ok")
            .await
            .expect("finish preflight step");
        tasks
            .update_phase(task_id, "executing")
            .await
            .expect("start executing phase");
        let deploy_step_id = tasks
            .start_step(StartTaskStepInput {
                task_id,
                node_id: None,
                step_key: "compose.deploy",
                title: "Compose deploy",
                command: "docker compose up -d",
            })
            .await
            .expect("start deploy step");
        tasks
            .finish_step(task_id, deploy_step_id, Some(0), "deploy ok")
            .await
            .expect("finish deploy step");
        tasks
            .finish_success(task_id, "release deploy", "done")
            .await
            .expect("finish task");

        let phases = tasks.task_phases(task_id).await.expect("task phases");
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].phase_key, "preflight");
        assert_eq!(phases[0].status, "success");
        assert_eq!(phases[1].phase_key, "executing");
        assert_eq!(phases[1].status, "success");

        let steps = tasks.task_steps(task_id).await.expect("task steps");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, preflight_step_id);
        assert_eq!(steps[0].phase_id, Some(phases[0].id));
        assert_eq!(steps[1].id, deploy_step_id);
        assert_eq!(steps[1].phase_id, Some(phases[1].id));
    }

    #[tokio::test]
    async fn cancel_queued_task_marks_canceled_and_writes_log() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "部署订单服务".to_owned(),
                app_id: None,
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");

        tasks
            .cancel_queued(task_id, "admin")
            .await
            .expect("cancel queued task");

        let detail = tasks.task_detail(task_id).await.expect("task detail");
        assert_eq!(detail.status, "canceled");
        assert_eq!(detail.summary, "admin 取消了排队任务");
        let logs = tasks.task_logs(task_id).await.expect("task logs");
        assert!(logs.iter().any(|log| log.content == "admin 取消了排队任务"));
    }

    #[tokio::test]
    async fn cancel_running_task_is_rejected() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "部署订单服务".to_owned(),
                app_id: None,
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");
        assert!(
            tasks
                .mark_running(task_id, "部署前预检", "preflight")
                .await
                .expect("mark running")
        );

        let err = tasks
            .cancel_queued(task_id, "admin")
            .await
            .expect_err("running task cannot be canceled");
        assert!(err.message().contains("只能取消等待中的任务"));
    }

    #[tokio::test]
    async fn deployment_run_records_config_revision_and_artifact_version() {
        let tasks = task_service().await;
        let app_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir)
            VALUES ('worker-bin', 'Worker', 'binary', 'binary', '/opt/worker')
            RETURNING id
            "#,
        )
        .fetch_one(&tasks.db)
        .await
        .expect("create app");
        let snapshot_id = sqlx::query_scalar::<_, i64>(
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
            VALUES (?1, 3, 'manual', '', 'RUST_LOG=info', 'v1.2.3', 'abc123', '{}')
            RETURNING id
            "#,
        )
        .bind(app_id)
        .fetch_one(&tasks.db)
        .await
        .expect("create config snapshot");
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "binary.restart".to_owned(),
                title: "重启 Worker".to_owned(),
                app_id: Some(app_id),
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create task");

        tasks
            .record_deployment_run(RecordDeploymentRunInput {
                app_id,
                task_id,
                release_id: None,
                deploy_action: "binary_restart",
                status: "success",
                message: "ok",
                config_snapshot_id: Some(snapshot_id),
                config_revision_no: 3,
                artifact_version: "v1.2.3",
            })
            .await
            .expect("record deployment run");

        let row = sqlx::query_as::<_, (i64, i64, String)>(
            r#"
            SELECT
                config_snapshot_id,
                config_revision_no,
                artifact_version
            FROM deployment_runs
            WHERE task_id = ?1
            "#,
        )
        .bind(task_id)
        .fetch_one(&tasks.db)
        .await
        .expect("load deployment run");

        assert_eq!(row.0, snapshot_id);
        assert_eq!(row.1, 3);
        assert_eq!(row.2, "v1.2.3");
    }

    #[tokio::test]
    async fn active_deploy_task_blocks_same_app_until_canceled() {
        let tasks = task_service().await;
        let app_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir)
            VALUES ('orders-api', '订单服务', 'compose', 'compose', '/opt/orders')
            RETURNING id
            "#,
        )
        .fetch_one(&tasks.db)
        .await
        .expect("create app");
        let first_task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "部署订单服务".to_owned(),
                app_id: Some(app_id),
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create first task");

        let err = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.restart".to_owned(),
                title: "重启订单服务".to_owned(),
                app_id: Some(app_id),
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect_err("same app active task should be rejected");
        assert!(err.message().contains("已有等待中或执行中的部署任务"));

        let active = tasks
            .active_app_task(app_id)
            .await
            .expect("active app task")
            .expect("active task exists");
        assert_eq!(active.id, first_task_id);
        assert_eq!(active.status, "queued");

        tasks
            .cancel_queued(first_task_id, "admin")
            .await
            .expect("cancel first task");
        let second_task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.restart".to_owned(),
                title: "重启订单服务".to_owned(),
                app_id: Some(app_id),
                release_id: None,
                node_id: None,
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create second task after cancel");

        let position = tasks
            .task_queue_position(second_task_id)
            .await
            .expect("queue position");
        assert_eq!(position.queued_before, 0);
        assert_eq!(position.running_before, 0);
    }
}
