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
    pub stream: String,
    pub content: String,
    pub created_at: String,
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
            SELECT id, stream, content, created_at
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
                node_id,
                status,
                created_by
            )
            VALUES (?1, ?2, ?3, ?4, 'queued', ?5)
            RETURNING id
            "#,
        )
        .bind(input.task_kind)
        .bind(input.title)
        .bind(input.app_id)
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
        sqlx::query(
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
        .execute(&self.db)
        .await?;
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
        self.append_log(task_id, "system", message).await
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
        app_id: i64,
        task_id: i64,
        deploy_action: &str,
        status: &str,
        message: &str,
    ) -> Result<(), TaskError> {
        sqlx::query(
            r#"
            INSERT INTO deployment_runs(
                app_id,
                task_id,
                deploy_action,
                status,
                finished_at,
                message
            )
            VALUES (
                ?1,
                ?2,
                ?3,
                ?4,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                ?5
            )
            "#,
        )
        .bind(app_id)
        .bind(task_id)
        .bind(deploy_action)
        .bind(status)
        .bind(message)
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
        "queued" | "preflight" | "preparing_files" | "executing" | "healthchecking"
        | "completed" | "failed" | "canceled" => Ok(phase),
        _ => Err(TaskError::InvalidState("任务阶段不支持".to_owned())),
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
    async fn cancel_queued_task_marks_canceled_and_writes_log() {
        let tasks = task_service().await;
        let task_id = tasks
            .create_task(CreateTaskInput {
                task_kind: "compose.up".to_owned(),
                title: "部署订单服务".to_owned(),
                app_id: None,
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
