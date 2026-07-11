use std::{
    cmp::Reverse,
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    Normal,
    Force,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentAction {
    Deploy,
    Skip,
    Start,
    Stop,
    Upgrade,
    Downgrade,
    Restore,
    ApplicationCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetUnitState {
    pub unit_id: i64,
    pub unit_key: String,
    pub unit_release_id: Option<i64>,
    pub release_version: Option<String>,
    pub release_version_code: Option<i64>,
    pub desired_status: String,
    pub stage_no: i64,
    pub unit_order: i64,
    pub removal_order: i64,
    pub target_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CurrentUnitNodeState {
    pub unit_id: i64,
    pub node_id: i64,
    pub runtime_status: String,
    pub active_unit_release_id: Option<i64>,
    pub active_version_code: Option<i64>,
    pub active_fingerprint: String,
    pub container_version_label: String,
}

#[derive(Debug, Clone)]
pub struct DeploymentPlanInput {
    pub app_id: i64,
    pub environment_id: i64,
    pub app_release_id: i64,
    pub config_revision_id: i64,
    pub mode: DeploymentMode,
    pub target_node_ids: Vec<i64>,
    pub target_units: Vec<TargetUnitState>,
    pub current_states: Vec<CurrentUnitNodeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentPlanItem {
    pub unit_id: i64,
    pub unit_key: String,
    pub unit_release_id: Option<i64>,
    pub release_version: Option<String>,
    pub stage_no: i64,
    pub unit_order: i64,
    pub removal_order: i64,
    pub action: DeploymentAction,
    pub reason: String,
    pub target_fingerprint: String,
    pub previous_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentPlan {
    pub app_id: i64,
    pub environment_id: i64,
    pub app_release_id: i64,
    pub config_revision_id: i64,
    pub mode: DeploymentMode,
    pub target_node_ids: Vec<i64>,
    pub items: Vec<DeploymentPlanItem>,
    pub plan_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentPlanError(String);

impl std::fmt::Display for DeploymentPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DeploymentPlanError {}

#[derive(Clone)]
pub struct DeploymentOrchestratorService {
    db: SqlitePool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentOrchestratorError {
    Validation(String),
    Conflict(String),
    NotFound(String),
    Database(String),
}

impl std::fmt::Display for DeploymentOrchestratorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::Conflict(message)
            | Self::NotFound(message)
            | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DeploymentOrchestratorError {}

impl From<sqlx::Error> for DeploymentOrchestratorError {
    fn from(error: sqlx::Error) -> Self {
        if let sqlx::Error::Database(database) = &error
            && (database.is_unique_violation()
                || database
                    .message()
                    .contains("active deployment task exists for app"))
        {
            return Self::Conflict("当前环境已有部署流程正在进行".to_owned());
        }
        Self::Database(error.to_string())
    }
}

impl From<DeploymentPlanError> for DeploymentOrchestratorError {
    fn from(error: DeploymentPlanError) -> Self {
        Self::Validation(error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct CreateDeploymentRunInput {
    pub environment_id: i64,
    pub app_release_id: i64,
    pub mode: DeploymentMode,
    pub expected_plan_hash: String,
    pub created_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedDeploymentRun {
    pub deployment_run_id: i64,
    pub task_id: i64,
    pub plan: DeploymentPlan,
}

#[derive(Debug, Clone)]
pub struct UnitExecutionContext {
    pub deployment_run_id: i64,
    pub task_id: i64,
    pub environment_id: i64,
    pub target_node_ids: Vec<i64>,
    pub item: DeploymentPlanItem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitExecutionOutcome {
    Success {
        summary: String,
    },
    Failed {
        failure_kind: String,
        summary: String,
        exit_code: Option<i32>,
    },
    CanceledUnknown {
        summary: String,
    },
}

#[async_trait]
pub trait DeploymentUnitExecutor: Send + Sync {
    async fn execute(&self, context: UnitExecutionContext) -> UnitExecutionOutcome;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRunOutcome {
    pub deployment_run_id: i64,
    pub status: String,
    pub summary: String,
}

#[derive(Debug, FromRow)]
struct PlanIdentityRow {
    app_id: i64,
    config_revision_id: i64,
    config_hash: String,
}

#[derive(Debug, FromRow)]
struct TargetUnitRow {
    unit_id: i64,
    unit_key: String,
    unit_release_id: Option<i64>,
    release_version: Option<String>,
    release_version_code: Option<i64>,
    desired_status: String,
    stage_no: i64,
    unit_order: i64,
    removal_order: i64,
    target_fingerprint: String,
}

#[derive(Debug, FromRow)]
struct CurrentUnitNodeRow {
    unit_id: i64,
    node_id: i64,
    runtime_status: String,
    active_unit_release_id: Option<i64>,
    active_version_code: Option<i64>,
    active_fingerprint: String,
    container_version_label: String,
}

#[derive(Debug, FromRow)]
struct ExecutionRunRow {
    task_id: i64,
    environment_id: i64,
    plan_hash: String,
    plan_json: String,
    max_parallel_units: i64,
}

impl DeploymentOrchestratorService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn preview(
        &self,
        environment_id: i64,
        app_release_id: i64,
        mode: DeploymentMode,
    ) -> Result<DeploymentPlan, DeploymentOrchestratorError> {
        let mut tx = self.db.begin().await?;
        let plan = load_deployment_plan(&mut tx, environment_id, app_release_id, mode).await?;
        tx.commit().await?;
        Ok(plan)
    }

    pub async fn create_run(
        &self,
        input: CreateDeploymentRunInput,
    ) -> Result<CreatedDeploymentRun, DeploymentOrchestratorError> {
        if input.expected_plan_hash.trim().is_empty() {
            return Err(DeploymentOrchestratorError::Validation(
                "确认部署时必须提交预览 plan hash".to_owned(),
            ));
        }
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let active_run: Option<(i64, String)> = sqlx::query_as(
            r#"
            SELECT id, status FROM environment_deployment_runs
            WHERE environment_id = ?1 AND status IN ('queued', 'running', 'reconciling')
            ORDER BY id DESC LIMIT 1
            "#,
        )
        .bind(input.environment_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((run_id, status)) = active_run {
            return Err(DeploymentOrchestratorError::Conflict(format!(
                "当前环境已有部署流程正在进行（部署 #{run_id}，状态 {status}）"
            )));
        }
        let plan = load_deployment_plan(
            &mut tx,
            input.environment_id,
            input.app_release_id,
            input.mode,
        )
        .await?;
        if plan.plan_hash != input.expected_plan_hash {
            return Err(DeploymentOrchestratorError::Conflict(
                "部署目标或运行状态已变化，请重新预览并确认".to_owned(),
            ));
        }
        let task_id = sqlx::query(
            r#"
            INSERT INTO operation_tasks(
                task_kind, title, app_id, release_id, environment_id, status, created_by
            ) VALUES ('release.deploy', ?1, ?2, ?3, ?4, 'queued', ?5)
            "#,
        )
        .bind(format!(
            "部署应用版本 #{} 到环境 #{}",
            input.app_release_id, input.environment_id
        ))
        .bind(plan.app_id)
        .bind(input.app_release_id)
        .bind(input.environment_id)
        .bind(input.created_by.trim())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        let plan_json = serde_json::to_string(&plan)
            .map_err(|error| DeploymentOrchestratorError::Validation(error.to_string()))?;
        let deployment_run_id = sqlx::query(
            r#"
            INSERT INTO environment_deployment_runs(
                app_id, environment_id, app_release_id, config_revision_id, task_id,
                deployment_mode, plan_hash, plan_json, status, created_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9)
            "#,
        )
        .bind(plan.app_id)
        .bind(plan.environment_id)
        .bind(plan.app_release_id)
        .bind(plan.config_revision_id)
        .bind(task_id)
        .bind(deployment_mode_name(plan.mode))
        .bind(&plan.plan_hash)
        .bind(plan_json)
        .bind(input.created_by.trim())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();

        let mut phase_numbers = plan
            .items
            .iter()
            .map(|item| item.stage_no)
            .collect::<Vec<_>>();
        phase_numbers.sort_unstable();
        phase_numbers.dedup();
        for (index, stage_no) in phase_numbers.into_iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO operation_task_phases(
                    task_id, phase_no, phase_key, title, status
                ) VALUES (?1, ?2, ?3, ?4, 'pending')
                "#,
            )
            .bind(task_id)
            .bind(index as i64 + 1)
            .bind(format!("deployment-stage-{stage_no}"))
            .bind(format!("部署阶段 {stage_no}"))
            .execute(&mut *tx)
            .await?;
        }
        for item in &plan.items {
            let status = if item.action == DeploymentAction::Skip {
                "skipped"
            } else {
                "pending"
            };
            sqlx::query(
                r#"
                INSERT INTO deployment_unit_run_results(
                    deployment_run_id, unit_id, unit_release_id, stage_no, action,
                    status, target_fingerprint, previous_fingerprint
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )
            .bind(deployment_run_id)
            .bind(item.unit_id)
            .bind(item.unit_release_id)
            .bind(item.stage_no)
            .bind(deployment_action_name(item.action))
            .bind(status)
            .bind(&item.target_fingerprint)
            .bind(&item.previous_fingerprint)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(CreatedDeploymentRun {
            deployment_run_id,
            task_id,
            plan,
        })
    }

    pub async fn execute_run(
        &self,
        deployment_run_id: i64,
        executor: Arc<dyn DeploymentUnitExecutor>,
    ) -> Result<DeploymentRunOutcome, DeploymentOrchestratorError> {
        let run = claim_deployment_run(&self.db, deployment_run_id).await?;
        let plan: DeploymentPlan = serde_json::from_str(&run.plan_json).map_err(|error| {
            DeploymentOrchestratorError::Validation(format!("部署计划快照损坏: {error}"))
        })?;
        if plan.plan_hash != run.plan_hash {
            return Err(DeploymentOrchestratorError::Conflict(
                "部署计划快照 hash 不一致，拒绝执行".to_owned(),
            ));
        }
        let mut waves = execution_waves(&plan.items);
        let mut failed = false;
        while let Some(items) = waves.pop_front() {
            if failed {
                break;
            }
            failed = execute_wave(
                &self.db,
                deployment_run_id,
                run.task_id,
                run.environment_id,
                &plan.target_node_ids,
                items,
                run.max_parallel_units.max(1) as usize,
                executor.clone(),
            )
            .await?;
        }
        if failed {
            sqlx::query(
                r#"
                UPDATE deployment_unit_run_results
                SET status = 'not_started', failure_kind = 'blocked_by_previous_failure',
                    failure_summary = '前序部署单元失败，本单元未启动',
                    finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                WHERE deployment_run_id = ?1 AND status = 'pending'
                "#,
            )
            .bind(deployment_run_id)
            .execute(&self.db)
            .await?;
        }
        finalize_deployment_run(&self.db, deployment_run_id, &plan).await
    }

    pub async fn reconcile_interrupted_runs(&self) -> Result<u64, DeploymentOrchestratorError> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let run_ids = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM environment_deployment_runs WHERE status = 'running' ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await?;
        for run_id in &run_ids {
            sqlx::query(
                r#"
                UPDATE deployment_unit_run_results
                SET status = 'canceled_unknown', failure_kind = 'process_interrupted',
                    failure_summary = '控制台进程重启，无法确认远端执行是否已经停止',
                    finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                WHERE deployment_run_id = ?1 AND status = 'running'
                "#,
            )
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                UPDATE environment_deployment_runs
                SET status = 'reconciling',
                    summary = '控制台进程重启，必须确认旧执行已停止后才能解锁',
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                WHERE id = ?1 AND status = 'running'
                "#,
            )
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                UPDATE operation_tasks
                SET status = 'failed', phase = 'failed',
                    summary = '控制台进程重启，环境部署进入待核对状态', exit_code = 1,
                    finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                WHERE id = (SELECT task_id FROM environment_deployment_runs WHERE id = ?1)
                  AND status = 'running'
                "#,
            )
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                r#"
                UPDATE app_environments
                SET runtime_status = 'unknown', last_deployment_status = 'running',
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                WHERE id = (SELECT environment_id FROM environment_deployment_runs WHERE id = ?1)
                "#,
            )
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(run_ids.len() as u64)
    }
}

async fn claim_deployment_run(
    db: &SqlitePool,
    deployment_run_id: i64,
) -> Result<ExecutionRunRow, DeploymentOrchestratorError> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let run = sqlx::query_as::<_, ExecutionRunRow>(
        r#"
        SELECT runs.task_id, runs.environment_id, runs.plan_hash, runs.plan_json,
               environments.max_parallel_units
        FROM environment_deployment_runs runs
        JOIN app_environments environments ON environments.id = runs.environment_id
        WHERE runs.id = ?1 AND runs.status = 'queued' AND runs.task_id IS NOT NULL
        "#,
    )
    .bind(deployment_run_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        DeploymentOrchestratorError::Conflict(
            "部署执行不存在、缺少任务或已被其他执行器领取".to_owned(),
        )
    })?;
    let run_claim = sqlx::query(
        r#"
        UPDATE environment_deployment_runs
        SET status = 'running', started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1 AND status = 'queued'
        "#,
    )
    .bind(deployment_run_id)
    .execute(&mut *tx)
    .await?;
    let task_claim = sqlx::query(
        r#"
        UPDATE operation_tasks
        SET status = 'running', phase = 'executing', command = 'execute immutable deployment plan',
            started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1 AND status = 'queued'
        "#,
    )
    .bind(run.task_id)
    .execute(&mut *tx)
    .await?;
    if run_claim.rows_affected() != 1 || task_claim.rows_affected() != 1 {
        return Err(DeploymentOrchestratorError::Conflict(
            "关联任务已取消或被其他执行器领取".to_owned(),
        ));
    }
    sqlx::query(
        "UPDATE app_environments SET last_deployment_status = 'running', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
    )
    .bind(run.environment_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(run)
}

fn execution_waves(items: &[DeploymentPlanItem]) -> VecDeque<Vec<DeploymentPlanItem>> {
    let mut waves = VecDeque::new();
    let executable = items
        .iter()
        .filter(|item| item.action != DeploymentAction::Skip);
    for item in executable {
        let wave_kind = item.action == DeploymentAction::Stop;
        let append = waves
            .back()
            .and_then(|wave: &Vec<DeploymentPlanItem>| wave.first())
            .is_some_and(|first| {
                (first.action == DeploymentAction::Stop) == wave_kind
                    && first.stage_no == item.stage_no
            });
        if append {
            waves
                .back_mut()
                .expect("wave exists when append is true")
                .push(item.clone());
        } else {
            waves.push_back(vec![item.clone()]);
        }
    }
    waves
}

#[allow(clippy::too_many_arguments)]
async fn execute_wave(
    db: &SqlitePool,
    deployment_run_id: i64,
    task_id: i64,
    environment_id: i64,
    target_node_ids: &[i64],
    items: Vec<DeploymentPlanItem>,
    max_parallel: usize,
    executor: Arc<dyn DeploymentUnitExecutor>,
) -> Result<bool, DeploymentOrchestratorError> {
    let mut pending = VecDeque::from(items);
    let mut running = tokio::task::JoinSet::new();
    let mut failure_seen = false;
    while !pending.is_empty() || !running.is_empty() {
        while !failure_seen && running.len() < max_parallel {
            let Some(item) = pending.pop_front() else {
                break;
            };
            let step_id = start_unit_result(db, deployment_run_id, task_id, &item).await?;
            let executor = executor.clone();
            let context = UnitExecutionContext {
                deployment_run_id,
                task_id,
                environment_id,
                target_node_ids: target_node_ids.to_vec(),
                item: item.clone(),
            };
            running.spawn(async move {
                let outcome = executor.execute(context).await;
                (item, step_id, outcome)
            });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let (item, step_id, outcome) = joined.map_err(|error| {
            DeploymentOrchestratorError::Database(format!("部署单元执行任务异常终止: {error}"))
        })?;
        if !matches!(outcome, UnitExecutionOutcome::Success { .. }) {
            failure_seen = true;
        }
        persist_unit_outcome(
            db,
            deployment_run_id,
            task_id,
            step_id,
            environment_id,
            target_node_ids,
            &item,
            &outcome,
        )
        .await?;
    }
    Ok(failure_seen)
}

async fn start_unit_result(
    db: &SqlitePool,
    deployment_run_id: i64,
    task_id: i64,
    item: &DeploymentPlanItem,
) -> Result<i64, DeploymentOrchestratorError> {
    let mut tx = db.begin().await?;
    let updated = sqlx::query(
        r#"
        UPDATE deployment_unit_run_results
        SET status = 'running', started_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE deployment_run_id = ?1 AND unit_id = ?2 AND status = 'pending'
        "#,
    )
    .bind(deployment_run_id)
    .bind(item.unit_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(DeploymentOrchestratorError::Conflict(format!(
            "部署单元 {} 当前状态不能开始执行",
            item.unit_key
        )));
    }
    let phase_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM operation_task_phases WHERE task_id = ?1 AND phase_key = ?2",
    )
    .bind(task_id)
    .bind(format!("deployment-stage-{}", item.stage_no))
    .fetch_optional(&mut *tx)
    .await?;
    let step_no: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(step_no), 0) + 1 FROM operation_task_steps WHERE task_id = ?1",
    )
    .bind(task_id)
    .fetch_one(&mut *tx)
    .await?;
    let step_id = sqlx::query(
        r#"
        INSERT INTO operation_task_steps(
            task_id, phase_id, step_no, step_key, title, command, status, started_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        "#,
    )
    .bind(task_id)
    .bind(phase_id)
    .bind(step_no)
    .bind(format!("unit-{}", item.unit_id))
    .bind(format!(
        "{}：{}",
        deployment_action_name(item.action),
        item.unit_key
    ))
    .bind(deployment_action_name(item.action))
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    tx.commit().await?;
    Ok(step_id)
}

#[allow(clippy::too_many_arguments)]
async fn persist_unit_outcome(
    db: &SqlitePool,
    deployment_run_id: i64,
    task_id: i64,
    step_id: i64,
    environment_id: i64,
    target_node_ids: &[i64],
    item: &DeploymentPlanItem,
    outcome: &UnitExecutionOutcome,
) -> Result<(), DeploymentOrchestratorError> {
    let (result_status, failure_kind, summary, exit_code, step_status) = match outcome {
        UnitExecutionOutcome::Success { summary } => {
            ("success", "", summary.as_str(), Some(0_i64), "success")
        }
        UnitExecutionOutcome::Failed {
            failure_kind,
            summary,
            exit_code,
        } => (
            "failed",
            failure_kind.as_str(),
            summary.as_str(),
            exit_code.map(i64::from),
            "failed",
        ),
        UnitExecutionOutcome::CanceledUnknown { summary } => (
            "canceled_unknown",
            "canceled_unknown",
            summary.as_str(),
            None,
            "failed",
        ),
    };
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"
        UPDATE deployment_unit_run_results
        SET status = ?3, failure_kind = ?4, failure_summary = ?5, exit_code = ?6,
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE deployment_run_id = ?1 AND unit_id = ?2 AND status = 'running'
        "#,
    )
    .bind(deployment_run_id)
    .bind(item.unit_id)
    .bind(result_status)
    .bind(failure_kind)
    .bind(summary)
    .bind(exit_code)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"
        UPDATE operation_task_steps
        SET status = ?3, exit_code = ?4,
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1 AND task_id = ?2
        "#,
    )
    .bind(step_id)
    .bind(task_id)
    .bind(step_status)
    .bind(exit_code)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO operation_task_logs(task_id, step_id, stream, content) VALUES (?1, ?2, 'system', ?3)",
    )
    .bind(task_id)
    .bind(step_id)
    .bind(summary)
    .execute(&mut *tx)
    .await?;
    if matches!(outcome, UnitExecutionOutcome::Success { .. }) {
        for node_id in target_node_ids {
            let (runtime_status, release_id, fingerprint, version_label) =
                if item.action == DeploymentAction::Stop {
                    ("stopped", None, "", "")
                } else {
                    (
                        "healthy",
                        item.unit_release_id,
                        item.target_fingerprint.as_str(),
                        item.release_version.as_deref().unwrap_or_default(),
                    )
                };
            sqlx::query(
                r#"
                INSERT INTO deployment_unit_runtime_states(
                    environment_id, unit_id, node_id, runtime_status,
                    active_unit_release_id, active_fingerprint, container_version_label,
                    message, last_deployment_run_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(environment_id, unit_id, node_id) DO UPDATE SET
                    runtime_status = excluded.runtime_status,
                    active_unit_release_id = excluded.active_unit_release_id,
                    active_fingerprint = excluded.active_fingerprint,
                    container_version_label = excluded.container_version_label,
                    message = excluded.message,
                    last_deployment_run_id = excluded.last_deployment_run_id,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                "#,
            )
            .bind(environment_id)
            .bind(item.unit_id)
            .bind(node_id)
            .bind(runtime_status)
            .bind(release_id)
            .bind(fingerprint)
            .bind(version_label)
            .bind(summary)
            .bind(deployment_run_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn finalize_deployment_run(
    db: &SqlitePool,
    deployment_run_id: i64,
    plan: &DeploymentPlan,
) -> Result<DeploymentRunOutcome, DeploymentOrchestratorError> {
    let counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        r#"
        SELECT
            SUM(CASE WHEN status IN ('success', 'skipped') THEN 1 ELSE 0 END),
            SUM(CASE WHEN status IN ('failed', 'not_started') THEN 1 ELSE 0 END),
            SUM(CASE WHEN status = 'canceled_unknown' THEN 1 ELSE 0 END),
            COUNT(*)
        FROM deployment_unit_run_results WHERE deployment_run_id = ?1
        "#,
    )
    .bind(deployment_run_id)
    .fetch_one(db)
    .await?;
    let status = if counts.1 == 0 && counts.2 == 0 {
        "success"
    } else if counts.0 > 0 {
        "partial_failed"
    } else if counts.2 > 0 {
        "canceled"
    } else {
        "all_failed"
    };
    let summary = format!(
        "共 {} 个部署单元：成功或跳过 {}，失败或未启动 {}，取消状态未知 {}",
        counts.3, counts.0, counts.1, counts.2
    );
    let task_status = match status {
        "success" => "success",
        "canceled" => "canceled",
        _ => "failed",
    };
    let environment_runtime = if status == "success" {
        if plan.items.iter().all(|item| item.unit_release_id.is_none()) {
            "stopped"
        } else {
            "running"
        }
    } else if status == "partial_failed" {
        "partial_unhealthy"
    } else {
        "unknown"
    };
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"
        UPDATE environment_deployment_runs
        SET status = ?2, summary = ?3,
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = ?1 AND status = 'running'
        "#,
    )
    .bind(deployment_run_id)
    .bind(status)
    .bind(&summary)
    .execute(&mut *tx)
    .await?;
    let task_phase = match task_status {
        "success" => "completed",
        "canceled" => "canceled",
        _ => "failed",
    };
    sqlx::query(
        r#"
        UPDATE operation_tasks
        SET status = ?2, phase = ?3, summary = ?4,
            exit_code = CASE WHEN ?2 = 'success' THEN 0 ELSE 1 END,
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE id = (SELECT task_id FROM environment_deployment_runs WHERE id = ?1)
        "#,
    )
    .bind(deployment_run_id)
    .bind(task_status)
    .bind(task_phase)
    .bind(&summary)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"
        UPDATE operation_task_phases
        SET status = CASE
                WHEN EXISTS(
                    SELECT 1 FROM deployment_unit_run_results results
                    JOIN environment_deployment_runs runs ON runs.id = results.deployment_run_id
                    WHERE runs.task_id = operation_task_phases.task_id
                      AND results.stage_no = CAST(substr(operation_task_phases.phase_key, 18) AS INTEGER)
                      AND results.status IN ('failed', 'canceled_unknown')
                ) THEN 'failed'
                WHEN EXISTS(
                    SELECT 1 FROM deployment_unit_run_results results
                    JOIN environment_deployment_runs runs ON runs.id = results.deployment_run_id
                    WHERE runs.task_id = operation_task_phases.task_id
                      AND results.stage_no = CAST(substr(operation_task_phases.phase_key, 18) AS INTEGER)
                      AND results.status = 'not_started'
                ) THEN 'skipped'
                ELSE 'success'
            END,
            finished_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE task_id = (SELECT task_id FROM environment_deployment_runs WHERE id = ?1)
        "#,
    )
    .bind(deployment_run_id)
    .execute(&mut *tx)
    .await?;
    if status == "success" {
        sqlx::query(
            r#"
            UPDATE app_environments
            SET current_app_release_id = ?2, current_config_revision_id = ?3,
                runtime_status = ?4, last_deployment_status = ?5,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(plan.environment_id)
        .bind(plan.app_release_id)
        .bind(plan.config_revision_id)
        .bind(environment_runtime)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE app_environments SET runtime_status = ?2, last_deployment_status = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1",
        )
        .bind(plan.environment_id)
        .bind(environment_runtime)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(DeploymentRunOutcome {
        deployment_run_id,
        status: status.to_owned(),
        summary,
    })
}

async fn load_deployment_plan(
    tx: &mut Transaction<'_, Sqlite>,
    environment_id: i64,
    app_release_id: i64,
    mode: DeploymentMode,
) -> Result<DeploymentPlan, DeploymentOrchestratorError> {
    let identity = sqlx::query_as::<_, PlanIdentityRow>(
        r#"
        SELECT environments.app_id, selections.config_revision_id, revisions.config_hash
        FROM app_environments environments
        JOIN app_releases releases
          ON releases.id = ?2 AND releases.app_id = environments.app_id
        JOIN application_release_manifests manifests
          ON manifests.app_release_id = releases.id AND manifests.immutable_status = 'ready'
        JOIN app_release_environment_configs selections
          ON selections.app_release_id = releases.id
         AND selections.environment_id = environments.id
        JOIN app_config_revisions revisions
          ON revisions.id = selections.config_revision_id
         AND revisions.app_id = environments.app_id
        WHERE environments.id = ?1 AND environments.status <> 'disabled'
        "#,
    )
    .bind(environment_id)
    .bind(app_release_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| {
        DeploymentOrchestratorError::NotFound(
            "环境、应用版本或对应配置版本不存在或不可部署".to_owned(),
        )
    })?;
    let target_node_ids = sqlx::query_scalar::<_, i64>(
        "SELECT node_id FROM app_environment_targets WHERE environment_id = ?1 ORDER BY node_id",
    )
    .bind(environment_id)
    .fetch_all(&mut **tx)
    .await?;
    let target_rows = sqlx::query_as::<_, TargetUnitRow>(
        r#"
        SELECT units.id AS unit_id, units.unit_key, release_units.unit_release_id,
               unit_releases.version AS release_version,
               unit_releases.version_code AS release_version_code,
               release_units.desired_status, release_units.stage_no,
               release_units.unit_order, release_units.removal_order,
               release_units.target_fingerprint
        FROM app_release_units release_units
        JOIN deployment_units units ON units.id = release_units.unit_id
        LEFT JOIN deployment_unit_releases unit_releases
          ON unit_releases.id = release_units.unit_release_id
        WHERE release_units.app_release_id = ?1 AND units.app_id = ?2
        ORDER BY release_units.stage_no, release_units.unit_order, units.id
        "#,
    )
    .bind(app_release_id)
    .bind(identity.app_id)
    .fetch_all(&mut **tx)
    .await?;
    let target_units = target_rows
        .into_iter()
        .map(|row| TargetUnitState {
            unit_id: row.unit_id,
            unit_key: row.unit_key,
            unit_release_id: row.unit_release_id,
            release_version: row.release_version,
            release_version_code: row.release_version_code,
            desired_status: row.desired_status,
            stage_no: row.stage_no,
            unit_order: row.unit_order,
            removal_order: row.removal_order,
            target_fingerprint: deployment_target_fingerprint(
                &row.target_fingerprint,
                &identity.config_hash,
            ),
        })
        .collect();
    let current_states = sqlx::query_as::<_, CurrentUnitNodeRow>(
        r#"
        SELECT states.unit_id, states.node_id, states.runtime_status,
               states.active_unit_release_id,
               unit_releases.version_code AS active_version_code,
               states.active_fingerprint, states.container_version_label
        FROM deployment_unit_runtime_states states
        LEFT JOIN deployment_unit_releases unit_releases
          ON unit_releases.id = states.active_unit_release_id
        WHERE states.environment_id = ?1
          AND states.node_id IN (
              SELECT node_id FROM app_environment_targets WHERE environment_id = ?1
          )
        ORDER BY states.unit_id, states.node_id
        "#,
    )
    .bind(environment_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| CurrentUnitNodeState {
        unit_id: row.unit_id,
        node_id: row.node_id,
        runtime_status: row.runtime_status,
        active_unit_release_id: row.active_unit_release_id,
        active_version_code: row.active_version_code,
        active_fingerprint: row.active_fingerprint,
        container_version_label: row.container_version_label,
    })
    .collect();
    Ok(build_deployment_plan(DeploymentPlanInput {
        app_id: identity.app_id,
        environment_id,
        app_release_id,
        config_revision_id: identity.config_revision_id,
        mode,
        target_node_ids,
        target_units,
        current_states,
    })?)
}

fn deployment_target_fingerprint(unit_fingerprint: &str, config_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update((unit_fingerprint.len() as u64).to_be_bytes());
    hasher.update(unit_fingerprint.as_bytes());
    hasher.update((config_hash.len() as u64).to_be_bytes());
    hasher.update(config_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

const fn deployment_mode_name(mode: DeploymentMode) -> &'static str {
    match mode {
        DeploymentMode::Normal => "normal",
        DeploymentMode::Force => "force",
    }
}

const fn deployment_action_name(action: DeploymentAction) -> &'static str {
    match action {
        DeploymentAction::Deploy => "deploy",
        DeploymentAction::Skip => "skip",
        DeploymentAction::Start => "start",
        DeploymentAction::Stop => "stop",
        DeploymentAction::Upgrade => "upgrade",
        DeploymentAction::Downgrade => "downgrade",
        DeploymentAction::Restore => "restore",
        DeploymentAction::ApplicationCheck => "application_check",
    }
}

#[derive(Serialize)]
struct PlanHashDocument<'a> {
    app_id: i64,
    environment_id: i64,
    app_release_id: i64,
    config_revision_id: i64,
    mode: DeploymentMode,
    target_node_ids: &'a [i64],
    items: &'a [DeploymentPlanItem],
}

pub fn build_deployment_plan(
    mut input: DeploymentPlanInput,
) -> Result<DeploymentPlan, DeploymentPlanError> {
    if input.target_node_ids.is_empty() {
        return Err(DeploymentPlanError(
            "部署环境至少需要一个目标节点".to_owned(),
        ));
    }
    input.target_node_ids.sort_unstable();
    input.target_node_ids.dedup();
    if input.target_units.is_empty() {
        return Err(DeploymentPlanError("应用版本不包含部署单元".to_owned()));
    }

    let mut current_by_unit_node = BTreeMap::new();
    for state in input.current_states {
        if current_by_unit_node
            .insert((state.unit_id, state.node_id), state)
            .is_some()
        {
            return Err(DeploymentPlanError(
                "同一部署单元和节点存在重复运行状态".to_owned(),
            ));
        }
    }
    let mut target_ids = BTreeMap::new();
    let mut items = Vec::with_capacity(input.target_units.len());
    for target in input.target_units {
        if target_ids.insert(target.unit_id, ()).is_some() {
            return Err(DeploymentPlanError("应用版本包含重复部署单元".to_owned()));
        }
        if !matches!(target.desired_status.as_str(), "active" | "disabled") {
            return Err(DeploymentPlanError(
                "部署单元目标状态必须是 active 或 disabled".to_owned(),
            ));
        }
        if target.desired_status == "active" && target.unit_release_id.is_none() {
            return Err(DeploymentPlanError("启用的部署单元缺少发布包".to_owned()));
        }
        let states = input
            .target_node_ids
            .iter()
            .filter_map(|node_id| current_by_unit_node.get(&(target.unit_id, *node_id)))
            .collect::<Vec<_>>();
        let previous_fingerprint = common_previous_fingerprint(&states);
        let (action, reason) =
            classify_action(input.mode, &target, &input.target_node_ids, &states);
        items.push(DeploymentPlanItem {
            unit_id: target.unit_id,
            unit_key: target.unit_key,
            unit_release_id: target.unit_release_id,
            release_version: target.release_version,
            stage_no: target.stage_no,
            unit_order: target.unit_order,
            removal_order: target.removal_order,
            action,
            reason,
            target_fingerprint: target.target_fingerprint,
            previous_fingerprint,
        });
    }

    items.sort_by_key(|item| match item.action {
        DeploymentAction::Stop => (
            0_i8,
            Reverse(item.stage_no),
            Reverse(item.removal_order),
            item.unit_id,
        ),
        _ => (
            1_i8,
            Reverse(-item.stage_no),
            Reverse(-item.unit_order),
            item.unit_id,
        ),
    });
    let document = PlanHashDocument {
        app_id: input.app_id,
        environment_id: input.environment_id,
        app_release_id: input.app_release_id,
        config_revision_id: input.config_revision_id,
        mode: input.mode,
        target_node_ids: &input.target_node_ids,
        items: &items,
    };
    let plan_json = serde_json::to_vec(&document)
        .map_err(|error| DeploymentPlanError(format!("生成部署计划失败: {error}")))?;
    let plan_hash = format!("{:x}", Sha256::digest(plan_json));
    Ok(DeploymentPlan {
        app_id: input.app_id,
        environment_id: input.environment_id,
        app_release_id: input.app_release_id,
        config_revision_id: input.config_revision_id,
        mode: input.mode,
        target_node_ids: input.target_node_ids,
        items,
        plan_hash,
    })
}

fn classify_action(
    mode: DeploymentMode,
    target: &TargetUnitState,
    target_node_ids: &[i64],
    states: &[&CurrentUnitNodeState],
) -> (DeploymentAction, String) {
    if target.desired_status == "disabled" {
        let already_stopped = states.len() == target_node_ids.len()
            && states.iter().all(|state| state.runtime_status == "stopped");
        return if already_stopped || states.is_empty() {
            (DeploymentAction::Skip, "部署单元已经停止".to_owned())
        } else {
            (DeploymentAction::Stop, "目标应用版本停用该单元".to_owned())
        };
    }
    if mode == DeploymentMode::Force {
        return (
            DeploymentAction::Deploy,
            "强制全量部署重新执行启用单元".to_owned(),
        );
    }
    if states.len() != target_node_ids.len() {
        if states.is_empty() {
            return (DeploymentAction::Start, "目标节点尚未部署该单元".to_owned());
        }
        return (
            DeploymentAction::Deploy,
            "部分目标节点缺少可信运行状态".to_owned(),
        );
    }
    let fully_matching = states.iter().all(|state| {
        state.runtime_status == "healthy"
            && state.active_unit_release_id == target.unit_release_id
            && state.active_fingerprint == target.target_fingerprint
            && Some(state.container_version_label.as_str()) == target.release_version.as_deref()
    });
    if fully_matching {
        return (
            DeploymentAction::Skip,
            "版本、配置、容器 label 和健康状态均与目标一致".to_owned(),
        );
    }
    if states
        .iter()
        .any(|state| !matches!(state.runtime_status.as_str(), "healthy" | "stopped"))
    {
        return (
            DeploymentAction::Deploy,
            "运行状态或健康探测不可信，不能跳过".to_owned(),
        );
    }
    if states.iter().all(|state| state.runtime_status == "stopped") {
        return (
            DeploymentAction::Restore,
            "部署单元已停止，需要恢复目标版本".to_owned(),
        );
    }
    let current_codes = states
        .iter()
        .filter_map(|state| state.active_version_code)
        .collect::<Vec<_>>();
    if let Some(target_code) = target.release_version_code {
        if current_codes.iter().all(|code| *code < target_code) {
            return (DeploymentAction::Upgrade, "目标发布包版本更高".to_owned());
        }
        if current_codes.iter().all(|code| *code > target_code) {
            return (DeploymentAction::Downgrade, "目标发布包版本更低".to_owned());
        }
    }
    (
        DeploymentAction::Deploy,
        "目标指纹、容器 label 或节点版本不一致".to_owned(),
    )
}

fn common_previous_fingerprint(states: &[&CurrentUnitNodeState]) -> String {
    let Some(first) = states.first() else {
        return String::new();
    };
    if states
        .iter()
        .all(|state| state.active_fingerprint == first.active_fingerprint)
    {
        first.active_fingerprint.clone()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};

    use super::*;

    fn target(unit_id: i64, status: &str, release_id: Option<i64>) -> TargetUnitState {
        TargetUnitState {
            unit_id,
            unit_key: format!("unit-{unit_id}"),
            unit_release_id: release_id,
            release_version: release_id.map(|id| format!("1.0.{id}")),
            release_version_code: release_id,
            desired_status: status.to_owned(),
            stage_no: unit_id,
            unit_order: 1,
            removal_order: 1,
            target_fingerprint: format!("target-{unit_id}"),
        }
    }

    fn healthy(unit_id: i64, release_id: i64) -> CurrentUnitNodeState {
        CurrentUnitNodeState {
            unit_id,
            node_id: 10,
            runtime_status: "healthy".to_owned(),
            active_unit_release_id: Some(release_id),
            active_version_code: Some(release_id),
            active_fingerprint: format!("target-{unit_id}"),
            container_version_label: format!("1.0.{release_id}"),
        }
    }

    #[test]
    fn normal_skips_only_fully_matching_healthy_units() {
        let targets = vec![
            target(1, "active", Some(100)),
            target(2, "active", Some(200)),
        ];
        let mut api = healthy(1, 100);
        api.runtime_status = "unhealthy".to_owned();
        let current = vec![api, healthy(2, 200)];

        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Normal,
            target_node_ids: vec![10],
            target_units: targets,
            current_states: current,
        })
        .expect("build plan");

        assert_eq!(plan.items[0].action, DeploymentAction::Deploy);
        assert_eq!(plan.items[1].action, DeploymentAction::Skip);
        assert!(!plan.plan_hash.is_empty());
    }

    #[test]
    fn force_redeploys_active_units_but_stops_disabled_units() {
        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Force,
            target_node_ids: vec![10],
            target_units: vec![target(1, "active", Some(100)), target(2, "disabled", None)],
            current_states: vec![healthy(1, 100), healthy(2, 200)],
        })
        .expect("build force plan");

        assert_eq!(plan.items[0].unit_id, 2);
        assert_eq!(plan.items[0].action, DeploymentAction::Stop);
        assert_eq!(plan.items[1].action, DeploymentAction::Deploy);
    }

    #[test]
    fn classifies_start_upgrade_downgrade_restore_and_unknown_probe() {
        let targets = vec![
            target(1, "active", Some(100)),
            target(2, "active", Some(200)),
            target(3, "active", Some(300)),
            target(4, "active", Some(400)),
        ];
        let mut stopped = healthy(2, 150);
        stopped.runtime_status = "stopped".to_owned();
        let mut newer = healthy(3, 350);
        newer.active_version_code = Some(350);
        let mut unknown = healthy(4, 400);
        unknown.runtime_status = "unknown".to_owned();
        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Normal,
            target_node_ids: vec![10],
            target_units: targets,
            current_states: vec![stopped, newer, unknown],
        })
        .expect("build plan");

        assert_eq!(plan.items[0].action, DeploymentAction::Start);
        assert_eq!(plan.items[1].action, DeploymentAction::Restore);
        assert_eq!(plan.items[2].action, DeploymentAction::Downgrade);
        assert_eq!(plan.items[3].action, DeploymentAction::Deploy);
    }

    #[test]
    fn requires_every_target_node_to_be_healthy_before_skip() {
        let plan = build_deployment_plan(DeploymentPlanInput {
            app_id: 1,
            environment_id: 2,
            app_release_id: 3,
            config_revision_id: 4,
            mode: DeploymentMode::Normal,
            target_node_ids: vec![10, 11],
            target_units: vec![target(1, "active", Some(100))],
            current_states: vec![healthy(1, 100)],
        })
        .expect("build plan");

        assert_eq!(plan.items[0].action, DeploymentAction::Deploy);
        assert!(plan.items[0].reason.contains("目标节点"));
    }

    #[tokio::test]
    async fn creates_immutable_run_and_blocks_same_environment() {
        let db = database().await;
        let fixture = DatabaseFixture::create(&db).await;
        let service = DeploymentOrchestratorService::new(db.clone());
        let preview = service
            .preview(
                fixture.environment_id,
                fixture.app_release_id,
                DeploymentMode::Normal,
            )
            .await
            .expect("preview deployment");

        let created = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Normal,
                expected_plan_hash: preview.plan_hash.clone(),
                created_by: "operator".to_owned(),
            })
            .await
            .expect("create deployment run");

        assert_eq!(created.plan, preview);
        let stored_plan: String =
            sqlx::query_scalar("SELECT plan_json FROM environment_deployment_runs WHERE id = ?1")
                .bind(created.deployment_run_id)
                .fetch_one(&db)
                .await
                .expect("load stored plan");
        assert_eq!(
            serde_json::from_str::<DeploymentPlan>(&stored_plan).expect("parse stored plan"),
            preview
        );
        let result_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deployment_unit_run_results WHERE deployment_run_id = ?1",
        )
        .bind(created.deployment_run_id)
        .fetch_one(&db)
        .await
        .expect("count unit results");
        assert_eq!(result_count, 1);

        let blocked = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Force,
                expected_plan_hash: preview.plan_hash,
                created_by: "operator".to_owned(),
            })
            .await
            .expect_err("active environment run must block another run");
        assert!(matches!(blocked, DeploymentOrchestratorError::Conflict(_)));
    }

    #[tokio::test]
    async fn rejects_stale_preview_after_runtime_state_changes() {
        let db = database().await;
        let fixture = DatabaseFixture::create(&db).await;
        let service = DeploymentOrchestratorService::new(db.clone());
        let preview = service
            .preview(
                fixture.environment_id,
                fixture.app_release_id,
                DeploymentMode::Normal,
            )
            .await
            .expect("preview deployment");
        sqlx::query(
            r#"
            INSERT INTO deployment_unit_runtime_states(
                environment_id, unit_id, node_id, runtime_status, active_unit_release_id,
                active_fingerprint, container_version_label
            ) VALUES (?1, ?2, ?3, 'healthy', ?4, ?5, '1.0.0')
            "#,
        )
        .bind(fixture.environment_id)
        .bind(fixture.unit_id)
        .bind(fixture.node_id)
        .bind(fixture.unit_release_id)
        .bind(&fixture.target_fingerprint)
        .execute(&db)
        .await
        .expect("change runtime state");

        let stale = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Normal,
                expected_plan_hash: preview.plan_hash,
                created_by: "operator".to_owned(),
            })
            .await
            .expect_err("stale plan hash must fail");
        assert!(matches!(stale, DeploymentOrchestratorError::Conflict(_)));
    }

    #[tokio::test]
    async fn executes_same_stage_with_configured_parallel_limit() {
        let db = database().await;
        let fixture = DatabaseFixture::create(&db).await;
        add_release_unit(&db, &fixture, "web", 1, 2).await;
        add_release_unit(&db, &fixture, "admin", 1, 3).await;
        sqlx::query("UPDATE app_environments SET max_parallel_units = 2 WHERE id = ?1")
            .bind(fixture.environment_id)
            .execute(&db)
            .await
            .expect("set parallel limit");
        let service = DeploymentOrchestratorService::new(db.clone());
        let preview = service
            .preview(
                fixture.environment_id,
                fixture.app_release_id,
                DeploymentMode::Force,
            )
            .await
            .expect("preview deployment");
        let run = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Force,
                expected_plan_hash: preview.plan_hash,
                created_by: "operator".to_owned(),
            })
            .await
            .expect("create run");
        let executor = Arc::new(RecordingExecutor::new(None));

        let outcome = service
            .execute_run(run.deployment_run_id, executor.clone())
            .await
            .expect("execute run");

        assert_eq!(outcome.status, "success");
        assert_eq!(executor.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(executor.calls.lock().expect("calls lock").len(), 3);
        let runtime_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM deployment_unit_runtime_states WHERE environment_id = ?1 AND runtime_status = 'healthy'",
        )
        .bind(fixture.environment_id)
        .fetch_one(&db)
        .await
        .expect("count runtime states");
        assert_eq!(runtime_count, 3);
    }

    #[tokio::test]
    async fn failure_waits_started_units_and_marks_later_stage_not_started() {
        let db = database().await;
        let fixture = DatabaseFixture::create(&db).await;
        let web_unit_id = add_release_unit(&db, &fixture, "web", 1, 2).await;
        let admin_unit_id = add_release_unit(&db, &fixture, "admin", 2, 1).await;
        sqlx::query("UPDATE app_environments SET max_parallel_units = 2 WHERE id = ?1")
            .bind(fixture.environment_id)
            .execute(&db)
            .await
            .expect("set parallel limit");
        let service = DeploymentOrchestratorService::new(db.clone());
        let preview = service
            .preview(
                fixture.environment_id,
                fixture.app_release_id,
                DeploymentMode::Force,
            )
            .await
            .expect("preview deployment");
        let run = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Force,
                expected_plan_hash: preview.plan_hash,
                created_by: "operator".to_owned(),
            })
            .await
            .expect("create run");
        let executor = Arc::new(RecordingExecutor::new(Some(fixture.unit_id)));

        let outcome = service
            .execute_run(run.deployment_run_id, executor.clone())
            .await
            .expect("execute run");

        assert_eq!(outcome.status, "partial_failed");
        let calls = executor.calls.lock().expect("calls lock").clone();
        assert!(calls.contains(&fixture.unit_id));
        assert!(calls.contains(&web_unit_id));
        assert!(!calls.contains(&admin_unit_id));
        let statuses = sqlx::query_as::<_, (i64, String)>(
            "SELECT unit_id, status FROM deployment_unit_run_results WHERE deployment_run_id = ?1 ORDER BY unit_id",
        )
        .bind(run.deployment_run_id)
        .fetch_all(&db)
        .await
        .expect("load unit statuses")
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        assert_eq!(
            statuses.get(&fixture.unit_id).map(String::as_str),
            Some("failed")
        );
        assert_eq!(
            statuses.get(&web_unit_id).map(String::as_str),
            Some("success")
        );
        assert_eq!(
            statuses.get(&admin_unit_id).map(String::as_str),
            Some("not_started")
        );
    }

    #[tokio::test]
    async fn interrupted_running_run_becomes_reconciling_and_keeps_lock() {
        let db = database().await;
        let fixture = DatabaseFixture::create(&db).await;
        let service = DeploymentOrchestratorService::new(db.clone());
        let preview = service
            .preview(
                fixture.environment_id,
                fixture.app_release_id,
                DeploymentMode::Force,
            )
            .await
            .expect("preview");
        let run = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Force,
                expected_plan_hash: preview.plan_hash,
                created_by: "operator".to_owned(),
            })
            .await
            .expect("create run");
        claim_deployment_run(&db, run.deployment_run_id)
            .await
            .expect("claim run");
        sqlx::query(
            "UPDATE deployment_unit_run_results SET status = 'running' WHERE deployment_run_id = ?1",
        )
        .bind(run.deployment_run_id)
        .execute(&db)
        .await
        .expect("mark unit running");

        assert_eq!(
            service
                .reconcile_interrupted_runs()
                .await
                .expect("reconcile"),
            1
        );
        let status: String =
            sqlx::query_scalar("SELECT status FROM environment_deployment_runs WHERE id = ?1")
                .bind(run.deployment_run_id)
                .fetch_one(&db)
                .await
                .expect("load run status");
        assert_eq!(status, "reconciling");
        let unit_status: String = sqlx::query_scalar(
            "SELECT status FROM deployment_unit_run_results WHERE deployment_run_id = ?1",
        )
        .bind(run.deployment_run_id)
        .fetch_one(&db)
        .await
        .expect("load unit status");
        assert_eq!(unit_status, "canceled_unknown");

        let blocked = service
            .create_run(CreateDeploymentRunInput {
                environment_id: fixture.environment_id,
                app_release_id: fixture.app_release_id,
                mode: DeploymentMode::Force,
                expected_plan_hash: run.plan.plan_hash,
                created_by: "operator".to_owned(),
            })
            .await
            .expect_err("reconciling run must retain lock");
        assert!(matches!(blocked, DeploymentOrchestratorError::Conflict(_)));
    }

    struct RecordingExecutor {
        active: AtomicUsize,
        max_active: AtomicUsize,
        fail_unit_id: Option<i64>,
        calls: Mutex<Vec<i64>>,
    }

    impl RecordingExecutor {
        fn new(fail_unit_id: Option<i64>) -> Self {
            Self {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                fail_unit_id,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl DeploymentUnitExecutor for RecordingExecutor {
        async fn execute(&self, context: UnitExecutionContext) -> UnitExecutionOutcome {
            self.calls
                .lock()
                .expect("calls lock")
                .push(context.item.unit_id);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.fail_unit_id == Some(context.item.unit_id) {
                UnitExecutionOutcome::Failed {
                    failure_kind: "command_failed".to_owned(),
                    summary: "模拟部署失败".to_owned(),
                    exit_code: Some(1),
                }
            } else {
                UnitExecutionOutcome::Success {
                    summary: "模拟部署成功".to_owned(),
                }
            }
        }
    }

    async fn add_release_unit(
        db: &SqlitePool,
        fixture: &DatabaseFixture,
        key: &str,
        stage_no: i64,
        unit_order: i64,
    ) -> i64 {
        let app_id: i64 = sqlx::query_scalar("SELECT app_id FROM app_environments WHERE id = ?1")
            .bind(fixture.environment_id)
            .fetch_one(db)
            .await
            .expect("load app id");
        let unit_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO deployment_units(app_id, unit_key, name, work_dir) VALUES (?1, ?2, ?2, ?3) RETURNING id",
        )
        .bind(app_id)
        .bind(key)
        .bind(format!("/srv/app/{key}"))
        .fetch_one(db)
        .await
        .expect("insert unit");
        let stage_id = if let Some(id) = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM deployment_pipeline_stages WHERE app_id = ?1 AND stage_no = ?2",
        )
        .bind(app_id)
        .bind(stage_no)
        .fetch_optional(db)
        .await
        .expect("load stage")
        {
            id
        } else {
            sqlx::query(
                "INSERT INTO deployment_pipeline_stages(app_id, stage_no, stage_key, name) VALUES (?1, ?2, ?3, ?3)",
            )
            .bind(app_id)
            .bind(stage_no)
            .bind(format!("stage-{stage_no}"))
            .execute(db)
            .await
            .expect("insert stage")
            .last_insert_rowid()
        };
        sqlx::query(
            "INSERT INTO deployment_pipeline_stage_units(stage_id, unit_id, unit_order, removal_order) VALUES (?1, ?2, ?3, ?3)",
        )
        .bind(stage_id)
        .bind(unit_id)
        .bind(unit_order)
        .execute(db)
        .await
        .expect("insert stage unit");
        let unit_release_id = sqlx::query(
            "INSERT INTO deployment_unit_releases(unit_id, version, version_code, package_name, checksum_sha256) VALUES (?1, '1.0.0', 100, ?2, ?3)",
        )
        .bind(unit_id)
        .bind(format!("{key}.tar.gz"))
        .bind(format!("{unit_id:064x}"))
        .execute(db)
        .await
        .expect("insert unit release")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO app_release_units(app_release_id, unit_id, unit_release_id, stage_no, unit_order, removal_order, target_fingerprint) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        )
        .bind(fixture.app_release_id)
        .bind(unit_id)
        .bind(unit_release_id)
        .bind(stage_no)
        .bind(unit_order)
        .bind(format!("target-{key}"))
        .execute(db)
        .await
        .expect("insert app release unit");
        unit_id
    }

    async fn database() -> SqlitePool {
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

    struct DatabaseFixture {
        environment_id: i64,
        app_release_id: i64,
        unit_id: i64,
        unit_release_id: i64,
        node_id: i64,
        target_fingerprint: String,
    }

    impl DatabaseFixture {
        async fn create(db: &SqlitePool) -> Self {
            let app_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('orchestrator-app', 'Orchestrator App', 'compose', 'compose', '/srv/app', 'ready') RETURNING id",
            )
            .fetch_one(db)
            .await
            .expect("insert app");
            let node_id = sqlx::query_scalar::<_, i64>("SELECT id FROM nodes ORDER BY id LIMIT 1")
                .fetch_one(db)
                .await
                .expect("load local node");
            let environment_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO app_environments(app_id, environment_key, name, status) VALUES (?1, 'production', '正式环境', 'ready') RETURNING id",
            )
            .bind(app_id)
            .fetch_one(db)
            .await
            .expect("insert environment");
            sqlx::query(
                "INSERT INTO app_environment_targets(environment_id, node_id) VALUES (?1, ?2)",
            )
            .bind(environment_id)
            .bind(node_id)
            .execute(db)
            .await
            .expect("insert target");
            let unit_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO deployment_units(app_id, unit_key, name, work_dir) VALUES (?1, 'api', 'API', '/srv/app/api') RETURNING id",
            )
            .bind(app_id)
            .fetch_one(db)
            .await
            .expect("insert unit");
            let stage_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO deployment_pipeline_stages(app_id, stage_no, stage_key, name) VALUES (?1, 1, 'api', 'API') RETURNING id",
            )
            .bind(app_id)
            .fetch_one(db)
            .await
            .expect("insert stage");
            sqlx::query(
                "INSERT INTO deployment_pipeline_stage_units(stage_id, unit_id) VALUES (?1, ?2)",
            )
            .bind(stage_id)
            .bind(unit_id)
            .execute(db)
            .await
            .expect("insert stage unit");
            let config_revision_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, config_hash) VALUES (?1, 100, '{}', '{}', 'config-hash') RETURNING id",
            )
            .bind(app_id)
            .fetch_one(db)
            .await
            .expect("insert config");
            let unit_release_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO deployment_unit_releases(unit_id, version, version_code, package_name, checksum_sha256) VALUES (?1, '1.0.0', 100, 'api.tar.gz', ?2) RETURNING id",
            )
            .bind(unit_id)
            .bind("a".repeat(64))
            .fetch_one(db)
            .await
            .expect("insert unit release");
            let app_release_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '1.0.0', 100, 'received', 'openapi') RETURNING id",
            )
            .bind(app_id)
            .fetch_one(db)
            .await
            .expect("insert app release");
            sqlx::query(
                "INSERT INTO application_release_manifests(app_release_id, manifest_hash, manifest_json) VALUES (?1, 'fixture-manifest', '{}')",
            )
            .bind(app_release_id)
            .execute(db)
            .await
            .expect("insert manifest");
            let base_fingerprint = "unit-target";
            sqlx::query(
                "INSERT INTO app_release_units(app_release_id, unit_id, unit_release_id, target_fingerprint) VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(app_release_id)
            .bind(unit_id)
            .bind(unit_release_id)
            .bind(base_fingerprint)
            .execute(db)
            .await
            .expect("insert release unit");
            sqlx::query(
                "INSERT INTO app_release_environment_configs(app_release_id, environment_id, config_revision_id) VALUES (?1, ?2, ?3)",
            )
            .bind(app_release_id)
            .bind(environment_id)
            .bind(config_revision_id)
            .execute(db)
            .await
            .expect("insert release environment");
            Self {
                environment_id,
                app_release_id,
                unit_id,
                unit_release_id,
                node_id,
                target_fingerprint: deployment_target_fingerprint(base_fingerprint, "config-hash"),
            }
        }
    }
}
