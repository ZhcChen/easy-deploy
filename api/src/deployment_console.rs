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
        assert!(detail.runs.is_empty());
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
