use sqlx::{FromRow, SqlitePool};

#[derive(Clone)]
pub struct DeploymentConsoleService {
    db: SqlitePool,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct ApplicationEnvironmentSummary {
    pub app_id: i64,
    pub environment_id: i64,
    pub environment_key: String,
    pub environment_name: String,
    pub environment_status: String,
    pub runtime_status: String,
    pub last_deployment_status: String,
    pub latest_release_id: Option<i64>,
    pub latest_version: Option<String>,
    pub latest_version_code: Option<i64>,
    pub active_run_id: Option<i64>,
    pub active_task_id: Option<i64>,
    pub active_run_status: Option<String>,
    pub unit_count: i64,
    pub target_count: i64,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct DeploymentUnitSummary {
    pub unit_id: i64,
    pub unit_key: String,
    pub unit_name: String,
    pub description: String,
    pub lifecycle_status: String,
    pub required: i64,
    pub work_dir: String,
    pub stage_no: i64,
    pub stage_name: String,
    pub unit_order: i64,
    pub latest_release_id: Option<i64>,
    pub latest_version: Option<String>,
    pub latest_version_code: Option<i64>,
    pub node_count: i64,
    pub healthy_count: i64,
    pub unhealthy_count: i64,
    pub deploying_count: i64,
    pub stopped_count: i64,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct ApplicationReleaseSummary {
    pub release_id: i64,
    pub version: String,
    pub version_code: i64,
    pub status: String,
    pub immutable_status: String,
    pub unit_count: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct DeploymentRunSummary {
    pub run_id: i64,
    pub task_id: Option<i64>,
    pub environment_id: i64,
    pub environment_name: String,
    pub release_id: i64,
    pub release_version: String,
    pub release_version_code: i64,
    pub deployment_mode: String,
    pub status: String,
    pub summary: String,
    pub success_count: i64,
    pub failed_count: i64,
    pub skipped_count: i64,
    pub pending_count: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct DeploymentRunDetailSummary {
    pub run_id: i64,
    pub app_id: i64,
    pub app_name: String,
    pub app_key: String,
    pub task_id: Option<i64>,
    pub task_title: Option<String>,
    pub environment_id: i64,
    pub environment_name: String,
    pub environment_key: String,
    pub release_id: i64,
    pub release_version: String,
    pub release_version_code: i64,
    pub config_revision_no: i64,
    pub deployment_mode: String,
    pub status: String,
    pub summary: String,
    pub created_by: String,
    pub plan_hash: String,
    pub snapshot_status: String,
    pub snapshot_deleted_at: Option<String>,
    pub snapshot_bytes: i64,
    pub log_bytes: i64,
    pub log_dropped_bytes: i64,
    pub log_truncated: bool,
    pub replayable: bool,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, FromRow, PartialEq, Eq)]
pub struct DeploymentUnitRunResultDetail {
    pub result_id: i64,
    pub unit_id: i64,
    pub unit_key: String,
    pub unit_name: String,
    pub unit_release_id: Option<i64>,
    pub release_version: Option<String>,
    pub release_version_code: Option<i64>,
    pub artifact_status: Option<String>,
    pub artifact_size_bytes: Option<i64>,
    pub stage_no: i64,
    pub action: String,
    pub status: String,
    pub failure_kind: String,
    pub failure_summary: String,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRunDetail {
    pub run: DeploymentRunDetailSummary,
    pub units: Vec<DeploymentUnitRunResultDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationDeploymentDetail {
    pub environments: Vec<ApplicationEnvironmentSummary>,
    pub units: Vec<DeploymentUnitSummary>,
    pub releases: Vec<ApplicationReleaseSummary>,
    pub runs: Vec<DeploymentRunSummary>,
}

impl DeploymentConsoleService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn application_environments(
        &self,
        app_id: i64,
    ) -> Result<Vec<ApplicationEnvironmentSummary>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT apps.id AS app_id,
                   environments.id AS environment_id,
                   environments.environment_key,
                   environments.name AS environment_name,
                   environments.status AS environment_status,
                   environments.runtime_status,
                   environments.last_deployment_status,
                   latest_release.id AS latest_release_id,
                   latest_release.version AS latest_version,
                   latest_release.version_code AS latest_version_code,
                   active_run.id AS active_run_id,
                   active_run.task_id AS active_task_id,
                   active_run.status AS active_run_status,
                   (SELECT COUNT(*) FROM deployment_units units
                    WHERE units.app_id = apps.id) AS unit_count,
                   (SELECT COUNT(*) FROM app_environment_targets targets
                    WHERE targets.environment_id = environments.id) AS target_count
            FROM apps
            JOIN app_environments environments ON environments.app_id = apps.id
            LEFT JOIN app_releases latest_release ON latest_release.id = (
                SELECT candidate.id FROM app_releases candidate
                JOIN application_release_manifests manifests
                  ON manifests.app_release_id = candidate.id
                WHERE candidate.app_id = apps.id AND manifests.immutable_status = 'ready'
                ORDER BY candidate.version_code DESC, candidate.id DESC LIMIT 1
            )
            LEFT JOIN environment_deployment_runs active_run ON active_run.id = (
                SELECT candidate.id FROM environment_deployment_runs candidate
                WHERE candidate.environment_id = environments.id
                  AND candidate.status IN ('queued', 'running', 'reconciling')
                ORDER BY candidate.id DESC LIMIT 1
            )
            WHERE apps.id = ?1
            ORDER BY environments.id
            "#,
        )
        .bind(app_id)
        .fetch_all(&self.db)
        .await
    }

    pub async fn environment_units(
        &self,
        app_id: i64,
        environment_id: i64,
    ) -> Result<Vec<DeploymentUnitSummary>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT units.id AS unit_id, units.unit_key, units.name AS unit_name,
                   units.description, units.lifecycle_status, units.required, units.work_dir,
                   COALESCE(stages.stage_no, 1) AS stage_no,
                   COALESCE(stages.name, '默认阶段') AS stage_name,
                   COALESCE(stage_units.unit_order, 1) AS unit_order,
                   latest_release.id AS latest_release_id,
                   latest_release.version AS latest_version,
                   latest_release.version_code AS latest_version_code,
                   COUNT(runtime.node_id) AS node_count,
                   SUM(CASE WHEN runtime.runtime_status = 'healthy' THEN 1 ELSE 0 END) AS healthy_count,
                   SUM(CASE WHEN runtime.runtime_status = 'unhealthy' THEN 1 ELSE 0 END) AS unhealthy_count,
                   SUM(CASE WHEN runtime.runtime_status = 'deploying' THEN 1 ELSE 0 END) AS deploying_count,
                   SUM(CASE WHEN runtime.runtime_status = 'stopped' THEN 1 ELSE 0 END) AS stopped_count
            FROM deployment_units units
            JOIN app_environments environments
              ON environments.app_id = units.app_id AND environments.id = ?2
            LEFT JOIN deployment_pipeline_stage_units stage_units ON stage_units.unit_id = units.id
            LEFT JOIN deployment_pipeline_stages stages ON stages.id = stage_units.stage_id
            LEFT JOIN deployment_unit_releases latest_release ON latest_release.id = (
                SELECT candidate.id FROM deployment_unit_releases candidate
                WHERE candidate.unit_id = units.id AND candidate.artifact_status = 'active'
                ORDER BY candidate.version_code DESC, candidate.id DESC LIMIT 1
            )
            LEFT JOIN deployment_unit_runtime_states runtime
              ON runtime.environment_id = environments.id AND runtime.unit_id = units.id
            WHERE units.app_id = ?1
            GROUP BY units.id, stages.id, stage_units.unit_order
            ORDER BY COALESCE(stages.stage_no, 1), COALESCE(stage_units.unit_order, 1), units.id
            "#,
        )
        .bind(app_id)
        .bind(environment_id)
        .fetch_all(&self.db)
        .await
    }

    pub async fn application_releases(
        &self,
        app_id: i64,
        limit: i64,
    ) -> Result<Vec<ApplicationReleaseSummary>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT releases.id AS release_id, releases.version, releases.version_code,
                   releases.status, manifests.immutable_status,
                   COUNT(release_units.unit_id) AS unit_count, releases.created_at
            FROM app_releases releases
            JOIN application_release_manifests manifests ON manifests.app_release_id = releases.id
            LEFT JOIN app_release_units release_units ON release_units.app_release_id = releases.id
            WHERE releases.app_id = ?1 AND manifests.immutable_status <> 'deleted'
            GROUP BY releases.id, manifests.immutable_status
            ORDER BY releases.version_code DESC, releases.id DESC
            LIMIT ?2
            "#,
        )
        .bind(app_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.db)
        .await
    }

    pub async fn deployment_runs(
        &self,
        app_id: i64,
        limit: i64,
    ) -> Result<Vec<DeploymentRunSummary>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT runs.id AS run_id, runs.task_id, runs.environment_id,
                   environments.name AS environment_name,
                   releases.id AS release_id, releases.version AS release_version,
                   releases.version_code AS release_version_code,
                   runs.deployment_mode, runs.status, runs.summary,
                   SUM(CASE WHEN results.status = 'success' THEN 1 ELSE 0 END) AS success_count,
                   SUM(CASE WHEN results.status IN ('failed', 'canceled_unknown') THEN 1 ELSE 0 END) AS failed_count,
                   SUM(CASE WHEN results.status = 'skipped' THEN 1 ELSE 0 END) AS skipped_count,
                   SUM(CASE WHEN results.status IN ('pending', 'running', 'not_started') THEN 1 ELSE 0 END) AS pending_count,
                   runs.created_at, runs.started_at, runs.finished_at
            FROM environment_deployment_runs runs
            JOIN app_environments environments ON environments.id = runs.environment_id
            JOIN app_releases releases ON releases.id = runs.app_release_id
            LEFT JOIN deployment_unit_run_results results ON results.deployment_run_id = runs.id
            WHERE runs.app_id = ?1
            GROUP BY runs.id
            ORDER BY runs.id DESC
            LIMIT ?2
            "#,
        )
        .bind(app_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.db)
        .await
    }

    pub async fn deployment_run_detail(
        &self,
        app_id: i64,
        deployment_run_id: i64,
    ) -> Result<Option<DeploymentRunDetail>, sqlx::Error> {
        let run = sqlx::query_as::<_, DeploymentRunDetailSummary>(
            r#"
            SELECT runs.id AS run_id, runs.app_id, apps.name AS app_name,
                   apps.app_key, runs.task_id, tasks.title AS task_title,
                   runs.environment_id, environments.name AS environment_name,
                   environments.environment_key,
                   releases.id AS release_id, releases.version AS release_version,
                   releases.version_code AS release_version_code,
                   configs.revision_no AS config_revision_no,
                   runs.deployment_mode, runs.status, runs.summary, runs.created_by,
                   runs.plan_hash, runs.snapshot_status, runs.snapshot_deleted_at,
                   CASE WHEN runs.snapshot_status = 'active'
                        THEN length(CAST(runs.plan_json AS BLOB)) ELSE 0 END AS snapshot_bytes,
                   COALESCE((
                       SELECT SUM(buffers.stored_bytes)
                       FROM deployment_step_log_buffers buffers
                       WHERE buffers.task_id = runs.task_id
                   ), 0) + COALESCE((
                       SELECT SUM(length(CAST(logs.content AS BLOB)))
                       FROM operation_task_logs logs
                       WHERE logs.task_id = runs.task_id
                   ), 0) AS log_bytes,
                   COALESCE((
                       SELECT budgets.dropped_bytes
                       FROM deployment_task_log_budgets budgets
                       WHERE budgets.task_id = runs.task_id
                   ), 0) AS log_dropped_bytes,
                   COALESCE((
                       SELECT budgets.truncated
                       FROM deployment_task_log_budgets budgets
                       WHERE budgets.task_id = runs.task_id
                   ), 0) AS log_truncated,
                   CASE WHEN runs.snapshot_status = 'active' AND NOT EXISTS (
                       SELECT 1
                       FROM deployment_unit_run_results results
                       LEFT JOIN deployment_unit_releases artifacts
                         ON artifacts.id = results.unit_release_id
                       WHERE results.deployment_run_id = runs.id
                         AND results.action NOT IN ('stop', 'application_check')
                         AND (results.unit_release_id IS NULL OR artifacts.artifact_status <> 'active')
                   ) THEN 1 ELSE 0 END AS replayable,
                   runs.created_at, runs.started_at, runs.finished_at
            FROM environment_deployment_runs runs
            JOIN apps ON apps.id = runs.app_id
            JOIN app_environments environments ON environments.id = runs.environment_id
            JOIN app_releases releases ON releases.id = runs.app_release_id
            JOIN app_config_revisions configs ON configs.id = runs.config_revision_id
            LEFT JOIN operation_tasks tasks ON tasks.id = runs.task_id
            WHERE runs.id = ?1 AND runs.app_id = ?2
            "#,
        )
        .bind(deployment_run_id)
        .bind(app_id)
        .fetch_optional(&self.db)
        .await?;
        let Some(run) = run else {
            return Ok(None);
        };
        let units = sqlx::query_as::<_, DeploymentUnitRunResultDetail>(
            r#"
            SELECT results.id AS result_id, results.unit_id, units.unit_key,
                   units.name AS unit_name, results.unit_release_id,
                   artifacts.version AS release_version,
                   artifacts.version_code AS release_version_code,
                   artifacts.artifact_status, artifacts.size_bytes AS artifact_size_bytes,
                   results.stage_no, results.action, results.status,
                   results.failure_kind, results.failure_summary, results.exit_code,
                   results.started_at, results.finished_at
            FROM deployment_unit_run_results results
            JOIN deployment_units units ON units.id = results.unit_id
            LEFT JOIN deployment_unit_releases artifacts ON artifacts.id = results.unit_release_id
            WHERE results.deployment_run_id = ?1
            ORDER BY results.stage_no, results.id
            "#,
        )
        .bind(deployment_run_id)
        .fetch_all(&self.db)
        .await?;
        Ok(Some(DeploymentRunDetail { run, units }))
    }

    pub async fn application_detail(
        &self,
        app_id: i64,
        selected_environment_id: Option<i64>,
    ) -> Result<ApplicationDeploymentDetail, sqlx::Error> {
        let environments = self.application_environments(app_id).await?;
        let environment_id = selected_environment_id
            .filter(|id| {
                environments
                    .iter()
                    .any(|environment| environment.environment_id == *id)
            })
            .or_else(|| {
                environments
                    .first()
                    .map(|environment| environment.environment_id)
            });
        let units = match environment_id {
            Some(environment_id) => self.environment_units(app_id, environment_id).await?,
            None => Vec::new(),
        };
        Ok(ApplicationDeploymentDetail {
            environments,
            units,
            releases: self.application_releases(app_id, 30).await?,
            runs: self.deployment_runs(app_id, 30).await?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn database() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("connect sqlite");
        sqlx::migrate!("./migrations")
            .run(&db)
            .await
            .expect("run migrations");
        db
    }

    #[tokio::test]
    async fn returns_environment_units_releases_and_structured_run_results() {
        let db = database().await;
        let node_id = sqlx::query(
            "INSERT INTO nodes(node_key, name, node_type, address, ssh_user, work_dir, status) VALUES ('node-a', '节点 A', 'ssh', '127.0.0.1', 'root', '/srv', 'online')",
        )
        .execute(&db)
        .await
        .expect("insert node")
        .last_insert_rowid();
        let app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('console-app', '控制台应用', 'compose', 'compose', '/srv/app', 'ready')",
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
        sqlx::query("INSERT INTO app_environment_targets(environment_id, node_id) VALUES (?1, ?2)")
            .bind(environment_id)
            .bind(node_id)
            .execute(&db)
            .await
            .expect("insert target");
        let unit_id = sqlx::query(
            "INSERT INTO deployment_units(app_id, unit_key, name, work_dir) VALUES (?1, 'api', 'API', '/srv/app/api')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert unit")
        .last_insert_rowid();
        let stage_id = sqlx::query(
            "INSERT INTO deployment_pipeline_stages(app_id, stage_no, stage_key, name) VALUES (?1, 1, 'backend', '后端服务')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert stage")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO deployment_pipeline_stage_units(stage_id, unit_id) VALUES (?1, ?2)",
        )
        .bind(stage_id)
        .bind(unit_id)
        .execute(&db)
        .await
        .expect("insert stage unit");
        let unit_release_id = sqlx::query(
            "INSERT INTO deployment_unit_releases(unit_id, version, version_code, package_name, checksum_sha256) VALUES (?1, '1.2.0', 100, 'api.tgz', 'unit-checksum')",
        )
        .bind(unit_id)
        .execute(&db)
        .await
        .expect("insert unit release")
        .last_insert_rowid();
        let app_release_id = sqlx::query(
            "INSERT INTO app_releases(app_id, version, version_code, status, source) VALUES (?1, '2.0.0', 100, 'received', 'openapi')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert app release")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO application_release_manifests(app_release_id, manifest_hash, manifest_json) VALUES (?1, 'console-manifest', '{}')",
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
        .bind(unit_release_id)
        .execute(&db)
        .await
        .expect("insert release unit");
        sqlx::query(
            "INSERT INTO deployment_unit_runtime_states(environment_id, unit_id, node_id, runtime_status, active_unit_release_id) VALUES (?1, ?2, ?3, 'healthy', ?4)",
        )
        .bind(environment_id)
        .bind(unit_id)
        .bind(node_id)
        .bind(unit_release_id)
        .execute(&db)
        .await
        .expect("insert runtime state");
        let config_revision_id = sqlx::query(
            "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, config_hash) VALUES (?1, 100, '{}', '{}', 'console-config')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert config revision")
        .last_insert_rowid();
        let task_id = sqlx::query(
            "INSERT INTO operation_tasks(task_kind, title, app_id, release_id, environment_id, status, created_by) VALUES ('release.deploy', '部署控制台应用', ?1, ?2, ?3, 'failed', 'operator')",
        )
        .bind(app_id)
        .bind(app_release_id)
        .bind(environment_id)
        .execute(&db)
        .await
        .expect("insert task")
        .last_insert_rowid();
        let step_id = sqlx::query(
            "INSERT INTO operation_task_steps(task_id, step_no, step_key, title, status) VALUES (?1, 1, 'deploy', '部署 API', 'failed')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert step")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO deployment_task_log_budgets(task_id, stored_bytes, received_bytes, dropped_bytes, max_bytes, truncated) VALUES (?1, 12, 20, 8, 100, 1)",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert log budget");
        sqlx::query(
            "INSERT INTO deployment_step_log_buffers(step_id, task_id, head_content, stored_bytes, received_bytes, dropped_bytes, truncated, finished) VALUES (?1, ?2, X'68656C6C6F', 5, 8, 3, 1, 1)",
        )
        .bind(step_id)
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert bounded log");
        sqlx::query(
            "INSERT INTO operation_task_logs(task_id, stream, content) VALUES (?1, 'system', '开始部署')",
        )
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert legacy log");
        let run_id = sqlx::query(
            "INSERT INTO environment_deployment_runs(app_id, environment_id, app_release_id, config_revision_id, task_id, deployment_mode, plan_hash, plan_json, status, summary, created_by) VALUES (?1, ?2, ?3, ?4, ?5, 'normal', 'console-plan', '{\"units\":[\"api\"]}', 'all_failed', 'API 部署失败', 'operator')",
        )
        .bind(app_id)
        .bind(environment_id)
        .bind(app_release_id)
        .bind(config_revision_id)
        .bind(task_id)
        .execute(&db)
        .await
        .expect("insert deployment run")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO deployment_unit_run_results(deployment_run_id, unit_id, unit_release_id, stage_no, action, status, failure_kind, failure_summary, exit_code) VALUES (?1, ?2, ?3, 1, 'deploy', 'failed', 'command_failed', 'compose up 失败', 17)",
        )
        .bind(run_id)
        .bind(unit_id)
        .bind(unit_release_id)
        .execute(&db)
        .await
        .expect("insert unit result");

        let service = DeploymentConsoleService::new(db.clone());
        let detail = service
            .application_detail(app_id, Some(environment_id))
            .await
            .expect("load detail");

        assert_eq!(detail.environments.len(), 1);
        assert_eq!(
            detail.environments[0].latest_version.as_deref(),
            Some("2.0.0")
        );
        assert_eq!(detail.environments[0].unit_count, 1);
        assert_eq!(detail.environments[0].target_count, 1);
        assert_eq!(detail.units.len(), 1);
        assert_eq!(detail.units[0].stage_name, "后端服务");
        assert_eq!(detail.units[0].latest_version.as_deref(), Some("1.2.0"));
        assert_eq!(detail.units[0].healthy_count, 1);
        assert_eq!(detail.releases.len(), 1);
        assert_eq!(detail.runs.len(), 1);
        assert_eq!(detail.runs[0].status, "all_failed");

        let run = service
            .deployment_run_detail(app_id, run_id)
            .await
            .expect("load deployment run detail")
            .expect("deployment run exists");
        assert_eq!(run.run.app_name, "控制台应用");
        assert_eq!(run.run.environment_name, "正式环境");
        assert_eq!(run.run.release_version, "2.0.0");
        assert_eq!(run.run.config_revision_no, 100);
        assert!(run.run.snapshot_bytes > 2);
        assert!(run.run.log_bytes >= 5);
        assert_eq!(run.run.log_dropped_bytes, 8);
        assert!(run.run.log_truncated);
        assert!(run.run.replayable);
        assert_eq!(run.units.len(), 1);
        assert_eq!(run.units[0].unit_key, "api");
        assert_eq!(run.units[0].release_version.as_deref(), Some("1.2.0"));
        assert_eq!(run.units[0].artifact_status.as_deref(), Some("active"));
        assert_eq!(run.units[0].exit_code, Some(17));
        assert_eq!(run.units[0].failure_summary, "compose up 失败");
    }

    #[tokio::test]
    async fn rejects_environment_from_another_application_when_selecting_units() {
        let db = database().await;
        let app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('app-a', 'A', 'compose', 'compose', '/srv/a', 'ready')",
        )
        .execute(&db)
        .await
        .expect("insert app")
        .last_insert_rowid();
        let other_app_id = sqlx::query(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('app-b', 'B', 'compose', 'compose', '/srv/b', 'ready')",
        )
        .execute(&db)
        .await
        .expect("insert other app")
        .last_insert_rowid();
        let other_environment_id = sqlx::query(
            "INSERT INTO app_environments(app_id, environment_key, name) VALUES (?1, 'production', '正式环境')",
        )
        .bind(other_app_id)
        .execute(&db)
        .await
        .expect("insert other environment")
        .last_insert_rowid();
        sqlx::query(
            "INSERT INTO deployment_units(app_id, unit_key, name) VALUES (?1, 'api', 'API')",
        )
        .bind(app_id)
        .execute(&db)
        .await
        .expect("insert unit");

        let units = DeploymentConsoleService::new(db)
            .environment_units(app_id, other_environment_id)
            .await
            .expect("query units");
        assert!(units.is_empty());
    }
}
