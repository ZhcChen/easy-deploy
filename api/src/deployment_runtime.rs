use std::{collections::BTreeMap, path::PathBuf};

use sqlx::{FromRow, SqlitePool};

use crate::{
    application_config::{ApplicationConfigService, ConfigUnit},
    deployment_orchestrator::{DeploymentAction, UnitExecutionContext},
};

#[derive(Clone)]
pub struct DeploymentRuntimeService {
    db: SqlitePool,
    configs: ApplicationConfigService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentRuntimeError {
    Validation(String),
    NotFound(String),
    Config(String),
    Database(String),
}

impl std::fmt::Display for DeploymentRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::NotFound(message)
            | Self::Config(message)
            | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DeploymentRuntimeError {}

impl From<sqlx::Error> for DeploymentRuntimeError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
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
}
