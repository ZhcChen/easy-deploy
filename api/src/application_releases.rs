use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

#[derive(Clone)]
pub struct ApplicationReleaseService {
    db: SqlitePool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationReleaseError {
    Validation(String),
    Conflict(String),
    NotFound(String),
    Database(String),
}

impl std::fmt::Display for ApplicationReleaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::Conflict(message)
            | Self::NotFound(message)
            | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ApplicationReleaseError {}

impl From<sqlx::Error> for ApplicationReleaseError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct RegisterUnitReleaseInput {
    pub unit_id: i64,
    pub version: String,
    pub package_name: String,
    pub package_path: String,
    pub extract_dir: String,
    pub checksum_sha256: String,
    pub size_bytes: i64,
    pub published_at: String,
    pub source: String,
    pub metadata: JsonValue,
    pub storage: UnitReleaseStorage,
}

#[derive(Debug, Clone)]
pub struct UnitReleaseStorage {
    pub provider: String,
    pub bucket: String,
    pub object_key: String,
    pub endpoint: String,
    pub object_version_id: String,
    pub integrity: String,
}

impl UnitReleaseStorage {
    pub fn local() -> Self {
        Self {
            provider: "local".to_owned(),
            bucket: String::new(),
            object_key: String::new(),
            endpoint: String::new(),
            object_version_id: String::new(),
            integrity: "local".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitReleaseResult {
    pub release_id: i64,
    pub unit_id: i64,
    pub version: String,
    pub version_code: i64,
    pub checksum_sha256: String,
}

#[derive(Debug, Clone)]
pub struct CreateApplicationReleaseInput {
    pub app_id: i64,
    pub version: String,
    pub base_app_release_id: Option<i64>,
    pub unit_changes: Vec<UnitReleaseChange>,
    pub environment_configs: Vec<EnvironmentConfigSelection>,
    pub created_by: String,
}

#[derive(Debug, Clone)]
pub struct UnitReleaseChange {
    pub unit_id: i64,
    pub unit_release_id: Option<i64>,
    pub desired_status: String,
}

impl UnitReleaseChange {
    pub fn active(unit_id: i64, unit_release_id: i64) -> Self {
        Self {
            unit_id,
            unit_release_id: Some(unit_release_id),
            desired_status: "active".to_owned(),
        }
    }

    pub fn disabled(unit_id: i64) -> Self {
        Self {
            unit_id,
            unit_release_id: None,
            desired_status: "disabled".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvironmentConfigSelection {
    pub environment_id: i64,
    pub config_revision_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestUnit {
    pub unit_id: i64,
    pub unit_release_id: Option<i64>,
    pub desired_status: String,
    pub stage_no: i64,
    pub unit_order: i64,
    pub removal_order: i64,
    pub target_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEnvironmentConfig {
    pub environment_id: i64,
    pub config_revision_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationReleaseResult {
    pub app_release_id: i64,
    pub version: String,
    pub version_code: i64,
    pub manifest_hash: String,
    pub units: Vec<ManifestUnit>,
    pub environment_configs: Vec<ManifestEnvironmentConfig>,
}

#[derive(Debug, FromRow)]
struct ConfiguredUnitRow {
    unit_id: i64,
    stage_no: Option<i64>,
    unit_order: Option<i64>,
    removal_order: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ManifestDocument<'a> {
    units: &'a [ManifestUnit],
    environment_configs: &'a [ManifestEnvironmentConfig],
}

impl ApplicationReleaseService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn register_unit_release(
        &self,
        input: RegisterUnitReleaseInput,
    ) -> Result<UnitReleaseResult, ApplicationReleaseError> {
        validate_version(&input.version)?;
        validate_checksum(&input.checksum_sha256)?;
        validate_unit_release_input(&input)?;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let unit_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deployment_units WHERE id = ?1)")
                .bind(input.unit_id)
                .fetch_one(&mut *tx)
                .await?;
        if !unit_exists {
            return Err(ApplicationReleaseError::NotFound(
                "deployment unit not found".to_owned(),
            ));
        }

        if let Some(existing) = sqlx::query_as::<_, (i64, i64, String)>(
            "SELECT id, version_code, checksum_sha256 FROM deployment_unit_releases WHERE unit_id = ?1 AND version = ?2",
        )
        .bind(input.unit_id)
        .bind(&input.version)
        .fetch_optional(&mut *tx)
        .await?
        {
            if existing.2 != input.checksum_sha256 {
                return Err(ApplicationReleaseError::Conflict(
                    "unit version already exists with a different checksum".to_owned(),
                ));
            }
            tx.commit().await?;
            return Ok(UnitReleaseResult {
                release_id: existing.0,
                unit_id: input.unit_id,
                version: input.version,
                version_code: existing.1,
                checksum_sha256: existing.2,
            });
        }

        let version_code = allocate_version_code(&mut tx, "unit_release", input.unit_id).await?;
        let metadata = serde_json::to_string(&input.metadata)
            .map_err(|error| ApplicationReleaseError::Validation(error.to_string()))?;
        let release_id = sqlx::query(
            r#"
            INSERT INTO deployment_unit_releases(
                unit_id, version, version_code, package_name, package_path, extract_dir,
                source, checksum_sha256, size_bytes, published_at, metadata,
                storage_provider, storage_bucket, storage_object_key, storage_endpoint,
                storage_object_version_id, storage_integrity
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            "#,
        )
        .bind(input.unit_id)
        .bind(&input.version)
        .bind(version_code)
        .bind(input.package_name.trim())
        .bind(input.package_path.trim())
        .bind(input.extract_dir.trim())
        .bind(input.source.trim())
        .bind(&input.checksum_sha256)
        .bind(input.size_bytes)
        .bind(input.published_at.trim())
        .bind(metadata)
        .bind(&input.storage.provider)
        .bind(&input.storage.bucket)
        .bind(&input.storage.object_key)
        .bind(&input.storage.endpoint)
        .bind(&input.storage.object_version_id)
        .bind(&input.storage.integrity)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        tx.commit().await?;
        Ok(UnitReleaseResult {
            release_id,
            unit_id: input.unit_id,
            version: input.version,
            version_code,
            checksum_sha256: input.checksum_sha256,
        })
    }

    pub async fn create_application_release(
        &self,
        input: CreateApplicationReleaseInput,
    ) -> Result<ApplicationReleaseResult, ApplicationReleaseError> {
        validate_version(&input.version)?;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        ensure_app_and_version_available(&mut tx, input.app_id, &input.version).await?;

        let configured_units = sqlx::query_as::<_, ConfiguredUnitRow>(
            r#"
            SELECT units.id AS unit_id, stages.stage_no, links.unit_order, links.removal_order
            FROM deployment_units units
            LEFT JOIN deployment_pipeline_stage_units links ON links.unit_id = units.id
            LEFT JOIN deployment_pipeline_stages stages ON stages.id = links.stage_id
            WHERE units.app_id = ?1
            ORDER BY COALESCE(stages.stage_no, 2147483647), COALESCE(links.unit_order, 2147483647), units.id
            "#,
        )
        .bind(input.app_id)
        .fetch_all(&mut *tx)
        .await?;
        if configured_units.is_empty() {
            return Err(ApplicationReleaseError::Validation(
                "application has no deployment units".to_owned(),
            ));
        }
        let configured_unit_ids = configured_units
            .iter()
            .map(|unit| unit.unit_id)
            .collect::<BTreeSet<_>>();

        let mut units = if let Some(base_id) = input.base_app_release_id {
            ensure_base_release(&mut tx, input.app_id, base_id).await?;
            load_manifest_units(&mut tx, base_id).await?
        } else {
            BTreeMap::new()
        };
        let mut changed_units = BTreeSet::new();
        for change in &input.unit_changes {
            if !changed_units.insert(change.unit_id) {
                return Err(ApplicationReleaseError::Validation(
                    "deployment unit change is duplicated".to_owned(),
                ));
            }
            if !configured_unit_ids.contains(&change.unit_id) {
                return Err(ApplicationReleaseError::Validation(
                    "deployment unit does not belong to application".to_owned(),
                ));
            }
            if !matches!(change.desired_status.as_str(), "active" | "disabled") {
                return Err(ApplicationReleaseError::Validation(
                    "desired unit status must be active or disabled".to_owned(),
                ));
            }
            let configured = configured_units
                .iter()
                .find(|unit| unit.unit_id == change.unit_id)
                .expect("configured unit id came from the same collection");
            let unit_release_id = match change.desired_status.as_str() {
                "active" => {
                    let release_id = change.unit_release_id.ok_or_else(|| {
                        ApplicationReleaseError::Validation(
                            "active deployment unit requires a unit release".to_owned(),
                        )
                    })?;
                    ensure_unit_release(&mut tx, change.unit_id, release_id).await?;
                    Some(release_id)
                }
                "disabled" => None,
                _ => unreachable!(),
            };
            let fingerprint =
                unit_target_fingerprint(&mut tx, unit_release_id, &change.desired_status).await?;
            units.insert(
                change.unit_id,
                ManifestUnit {
                    unit_id: change.unit_id,
                    unit_release_id,
                    desired_status: change.desired_status.clone(),
                    stage_no: configured.stage_no.unwrap_or(1),
                    unit_order: configured.unit_order.unwrap_or(1),
                    removal_order: configured.removal_order.unwrap_or(1),
                    target_fingerprint: fingerprint,
                },
            );
        }
        if units.keys().copied().collect::<BTreeSet<_>>() != configured_unit_ids {
            return Err(ApplicationReleaseError::Validation(
                "application release must resolve every configured deployment unit".to_owned(),
            ));
        }

        let mut environment_configs = if let Some(base_id) = input.base_app_release_id {
            load_environment_configs(&mut tx, base_id).await?
        } else {
            BTreeMap::new()
        };
        let mut changed_environments = BTreeSet::new();
        for selection in &input.environment_configs {
            if !changed_environments.insert(selection.environment_id) {
                return Err(ApplicationReleaseError::Validation(
                    "environment config selection is duplicated".to_owned(),
                ));
            }
            ensure_environment_config(
                &mut tx,
                input.app_id,
                selection.environment_id,
                selection.config_revision_id,
            )
            .await?;
            environment_configs.insert(selection.environment_id, selection.config_revision_id);
        }
        let configured_environment_ids = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM app_environments WHERE app_id = ?1 AND status <> 'disabled' ORDER BY id",
        )
        .bind(input.app_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect::<BTreeSet<_>>();
        if environment_configs.keys().copied().collect::<BTreeSet<_>>()
            != configured_environment_ids
        {
            return Err(ApplicationReleaseError::Validation(
                "application release must resolve every enabled environment config".to_owned(),
            ));
        }

        let units = units.into_values().collect::<Vec<_>>();
        let environment_configs = environment_configs
            .into_iter()
            .map(
                |(environment_id, config_revision_id)| ManifestEnvironmentConfig {
                    environment_id,
                    config_revision_id,
                },
            )
            .collect::<Vec<_>>();
        let manifest_json = serde_json::to_string(&ManifestDocument {
            units: &units,
            environment_configs: &environment_configs,
        })
        .map_err(|error| ApplicationReleaseError::Validation(error.to_string()))?;
        let manifest_hash = sha256_hex(manifest_json.as_bytes());
        let duplicate_manifest: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM application_release_manifests WHERE manifest_hash = ?1)",
        )
        .bind(&manifest_hash)
        .fetch_one(&mut *tx)
        .await?;
        if duplicate_manifest {
            return Err(ApplicationReleaseError::Conflict(
                "an identical application release manifest already exists".to_owned(),
            ));
        }

        let version_code = allocate_version_code(&mut tx, "app_release", input.app_id).await?;
        let app_release_id = sqlx::query(
            r#"
            INSERT INTO app_releases(
                app_id, version, version_code, status, source, checksum_sha256,
                metadata, published_at
            )
            VALUES (?1, ?2, ?3, 'received', 'openapi', ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            "#,
        )
        .bind(input.app_id)
        .bind(&input.version)
        .bind(version_code)
        .bind(&manifest_hash)
        .bind(serde_json::json!({"kind": "application_manifest"}).to_string())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query(
            r#"
            INSERT INTO application_release_manifests(
                app_release_id, base_app_release_id, manifest_hash, manifest_json, created_by
            )
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
        )
        .bind(app_release_id)
        .bind(input.base_app_release_id)
        .bind(&manifest_hash)
        .bind(&manifest_json)
        .bind(input.created_by.trim())
        .execute(&mut *tx)
        .await?;
        for unit in &units {
            sqlx::query(
                r#"
                INSERT INTO app_release_units(
                    app_release_id, unit_id, unit_release_id, desired_status,
                    stage_no, unit_order, removal_order, target_fingerprint
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )
            .bind(app_release_id)
            .bind(unit.unit_id)
            .bind(unit.unit_release_id)
            .bind(&unit.desired_status)
            .bind(unit.stage_no)
            .bind(unit.unit_order)
            .bind(unit.removal_order)
            .bind(&unit.target_fingerprint)
            .execute(&mut *tx)
            .await?;
        }
        for environment in &environment_configs {
            sqlx::query(
                "INSERT INTO app_release_environment_configs(app_release_id, environment_id, config_revision_id) VALUES (?1, ?2, ?3)",
            )
            .bind(app_release_id)
            .bind(environment.environment_id)
            .bind(environment.config_revision_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(ApplicationReleaseResult {
            app_release_id,
            version: input.version,
            version_code,
            manifest_hash,
            units,
            environment_configs,
        })
    }
}

async fn allocate_version_code(
    tx: &mut Transaction<'_, Sqlite>,
    scope_kind: &str,
    scope_id: i64,
) -> Result<i64, ApplicationReleaseError> {
    sqlx::query(
        "INSERT INTO version_counters(scope_kind, scope_id, next_value) VALUES (?1, ?2, 100) ON CONFLICT(scope_kind, scope_id) DO NOTHING",
    )
    .bind(scope_kind)
    .bind(scope_id)
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_scalar(
        r#"
        UPDATE version_counters
        SET next_value = next_value + 1,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        WHERE scope_kind = ?1 AND scope_id = ?2
        RETURNING next_value - 1
        "#,
    )
    .bind(scope_kind)
    .bind(scope_id)
    .fetch_one(&mut **tx)
    .await?)
}

async fn ensure_app_and_version_available(
    tx: &mut Transaction<'_, Sqlite>,
    app_id: i64,
    version: &str,
) -> Result<(), ApplicationReleaseError> {
    let app_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM apps WHERE id = ?1)")
        .bind(app_id)
        .fetch_one(&mut **tx)
        .await?;
    if !app_exists {
        return Err(ApplicationReleaseError::NotFound(
            "application not found".to_owned(),
        ));
    }
    let version_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM app_releases WHERE app_id = ?1 AND version = ?2)",
    )
    .bind(app_id)
    .bind(version)
    .fetch_one(&mut **tx)
    .await?;
    if version_exists {
        return Err(ApplicationReleaseError::Conflict(
            "application version already exists".to_owned(),
        ));
    }
    Ok(())
}

async fn ensure_base_release(
    tx: &mut Transaction<'_, Sqlite>,
    app_id: i64,
    base_id: i64,
) -> Result<(), ApplicationReleaseError> {
    let valid: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM app_releases releases
            JOIN application_release_manifests manifests ON manifests.app_release_id = releases.id
            WHERE releases.id = ?1 AND releases.app_id = ?2 AND manifests.immutable_status = 'ready'
        )
        "#,
    )
    .bind(base_id)
    .bind(app_id)
    .fetch_one(&mut **tx)
    .await?;
    if valid {
        Ok(())
    } else {
        Err(ApplicationReleaseError::Validation(
            "base application release is not a ready release of this application".to_owned(),
        ))
    }
}

async fn load_manifest_units(
    tx: &mut Transaction<'_, Sqlite>,
    app_release_id: i64,
) -> Result<BTreeMap<i64, ManifestUnit>, ApplicationReleaseError> {
    let rows = sqlx::query_as::<_, (i64, Option<i64>, String, i64, i64, i64, String)>(
        r#"
        SELECT unit_id, unit_release_id, desired_status, stage_no, unit_order,
               removal_order, target_fingerprint
        FROM app_release_units WHERE app_release_id = ?1 ORDER BY stage_no, unit_order, unit_id
        "#,
    )
    .bind(app_release_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.0,
                ManifestUnit {
                    unit_id: row.0,
                    unit_release_id: row.1,
                    desired_status: row.2,
                    stage_no: row.3,
                    unit_order: row.4,
                    removal_order: row.5,
                    target_fingerprint: row.6,
                },
            )
        })
        .collect())
}

async fn load_environment_configs(
    tx: &mut Transaction<'_, Sqlite>,
    app_release_id: i64,
) -> Result<BTreeMap<i64, i64>, ApplicationReleaseError> {
    Ok(sqlx::query_as::<_, (i64, i64)>(
        "SELECT environment_id, config_revision_id FROM app_release_environment_configs WHERE app_release_id = ?1 ORDER BY environment_id",
    )
    .bind(app_release_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect())
}

async fn ensure_unit_release(
    tx: &mut Transaction<'_, Sqlite>,
    unit_id: i64,
    unit_release_id: i64,
) -> Result<(), ApplicationReleaseError> {
    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deployment_unit_releases WHERE id = ?1 AND unit_id = ?2 AND artifact_status = 'active')",
    )
    .bind(unit_release_id)
    .bind(unit_id)
    .fetch_one(&mut **tx)
    .await?;
    if valid {
        Ok(())
    } else {
        Err(ApplicationReleaseError::Validation(
            "unit release is not an active release of the selected deployment unit".to_owned(),
        ))
    }
}

async fn ensure_environment_config(
    tx: &mut Transaction<'_, Sqlite>,
    app_id: i64,
    environment_id: i64,
    config_revision_id: i64,
) -> Result<(), ApplicationReleaseError> {
    let valid: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM app_environments environments
            JOIN app_config_revisions revisions ON revisions.id = ?3 AND revisions.app_id = environments.app_id
            WHERE environments.id = ?2 AND environments.app_id = ?1 AND environments.status <> 'disabled'
        )
        "#,
    )
    .bind(app_id)
    .bind(environment_id)
    .bind(config_revision_id)
    .fetch_one(&mut **tx)
    .await?;
    if valid {
        Ok(())
    } else {
        Err(ApplicationReleaseError::Validation(
            "environment and config revision must belong to the application".to_owned(),
        ))
    }
}

async fn unit_target_fingerprint(
    tx: &mut Transaction<'_, Sqlite>,
    unit_release_id: Option<i64>,
    desired_status: &str,
) -> Result<String, ApplicationReleaseError> {
    let checksum = if let Some(release_id) = unit_release_id {
        sqlx::query_scalar::<_, String>(
            "SELECT checksum_sha256 FROM deployment_unit_releases WHERE id = ?1",
        )
        .bind(release_id)
        .fetch_one(&mut **tx)
        .await?
    } else {
        String::new()
    };
    Ok(sha256_hex(
        format!("{desired_status}:{checksum}").as_bytes(),
    ))
}

fn validate_unit_release_input(
    input: &RegisterUnitReleaseInput,
) -> Result<(), ApplicationReleaseError> {
    if input.package_name.trim().is_empty() {
        return Err(ApplicationReleaseError::Validation(
            "unit release package name is required".to_owned(),
        ));
    }
    if input.size_bytes < 0 {
        return Err(ApplicationReleaseError::Validation(
            "unit release size cannot be negative".to_owned(),
        ));
    }
    if !matches!(input.source.as_str(), "openapi" | "web" | "migration") {
        return Err(ApplicationReleaseError::Validation(
            "unsupported unit release source".to_owned(),
        ));
    }
    match input.storage.provider.as_str() {
        "local" if input.storage.integrity == "local" => Ok(()),
        "aliyun_oss"
            if matches!(
                input.storage.integrity.as_str(),
                "unique_key" | "version_pinned"
            ) && !input.storage.bucket.trim().is_empty()
                && !input.storage.object_key.trim().is_empty() =>
        {
            Ok(())
        }
        _ => Err(ApplicationReleaseError::Validation(
            "unit release storage metadata is incomplete or unsupported".to_owned(),
        )),
    }
}

fn validate_version(version: &str) -> Result<(), ApplicationReleaseError> {
    let value = version.trim();
    let parts = value.split('.').collect::<Vec<_>>();
    let valid = parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
                && part.parse::<u64>().is_ok()
        });
    if valid {
        Ok(())
    } else {
        Err(ApplicationReleaseError::Validation(
            "version must use x.y.z numeric format".to_owned(),
        ))
    }
}

fn validate_checksum(checksum: &str) -> Result<(), ApplicationReleaseError> {
    if checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ApplicationReleaseError::Validation(
            "checksum_sha256 must be 64 hexadecimal characters".to_owned(),
        ))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};

    use super::*;

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

    #[tokio::test]
    async fn creates_complete_release_and_inherits_unchanged_units() {
        let db = database().await;
        let fixture = ReleaseFixture::create(&db).await;
        let service = ApplicationReleaseService::new(db.clone());

        let api_v1 = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "1.0.0", "a"))
            .await
            .expect("register api v1");
        let web_v1 = service
            .register_unit_release(fixture.unit_release_input(fixture.web_unit_id, "1.0.0", "b"))
            .await
            .expect("register web v1");
        assert_eq!(api_v1.version_code, 100);
        assert_eq!(web_v1.version_code, 100);

        let first = service
            .create_application_release(CreateApplicationReleaseInput {
                app_id: fixture.app_id,
                version: "1.0.0".to_owned(),
                base_app_release_id: None,
                unit_changes: vec![
                    UnitReleaseChange::active(fixture.api_unit_id, api_v1.release_id),
                    UnitReleaseChange::active(fixture.web_unit_id, web_v1.release_id),
                ],
                environment_configs: vec![EnvironmentConfigSelection {
                    environment_id: fixture.environment_id,
                    config_revision_id: fixture.config_revision_id,
                }],
                created_by: "ci".to_owned(),
            })
            .await
            .expect("create first app release");
        assert_eq!(first.version_code, 100);
        assert_eq!(first.units.len(), 2);

        let api_v2 = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "1.1.0", "c"))
            .await
            .expect("register api v2");
        let second = service
            .create_application_release(CreateApplicationReleaseInput {
                app_id: fixture.app_id,
                version: "1.1.0".to_owned(),
                base_app_release_id: Some(first.app_release_id),
                unit_changes: vec![UnitReleaseChange::active(
                    fixture.api_unit_id,
                    api_v2.release_id,
                )],
                environment_configs: vec![],
                created_by: "ci".to_owned(),
            })
            .await
            .expect("create inherited release");

        assert_eq!(second.version_code, 101);
        assert_eq!(second.units.len(), 2);
        assert!(second.units.iter().any(|unit| {
            unit.unit_id == fixture.web_unit_id && unit.unit_release_id == Some(web_v1.release_id)
        }));
        assert_eq!(second.environment_configs.len(), 1);
    }

    #[tokio::test]
    async fn rejects_cross_application_release_references_and_version_overwrite() {
        let db = database().await;
        let fixture = ReleaseFixture::create(&db).await;
        let other = ReleaseFixture::create_named(&db, "other").await;
        let service = ApplicationReleaseService::new(db);
        let other_release = service
            .register_unit_release(other.unit_release_input(other.api_unit_id, "1.0.0", "d"))
            .await
            .expect("register other release");

        let error = service
            .create_application_release(CreateApplicationReleaseInput {
                app_id: fixture.app_id,
                version: "1.0.0".to_owned(),
                base_app_release_id: None,
                unit_changes: vec![
                    UnitReleaseChange::active(fixture.api_unit_id, other_release.release_id),
                    UnitReleaseChange::disabled(fixture.web_unit_id),
                ],
                environment_configs: vec![EnvironmentConfigSelection {
                    environment_id: fixture.environment_id,
                    config_revision_id: fixture.config_revision_id,
                }],
                created_by: "ci".to_owned(),
            })
            .await
            .expect_err("cross-app unit release must fail");
        assert!(matches!(error, ApplicationReleaseError::Validation(_)));

        let first = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "2.0.0", "e"))
            .await
            .expect("register version");
        let same = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "2.0.0", "e"))
            .await
            .expect("same checksum is idempotent");
        assert_eq!(same.release_id, first.release_id);
        let conflict = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "2.0.0", "f"))
            .await
            .expect_err("different checksum cannot overwrite");
        assert!(matches!(conflict, ApplicationReleaseError::Conflict(_)));
    }

    #[tokio::test]
    async fn concurrent_unit_releases_receive_distinct_monotonic_codes() {
        let db = database().await;
        let fixture = ReleaseFixture::create(&db).await;
        let service = ApplicationReleaseService::new(db);
        let first_service = service.clone();
        let second_service = service.clone();
        let first_input = fixture.unit_release_input(fixture.api_unit_id, "3.0.0", "1");
        let second_input = fixture.unit_release_input(fixture.api_unit_id, "3.1.0", "2");

        let (first, second) = tokio::join!(
            first_service.register_unit_release(first_input),
            second_service.register_unit_release(second_input)
        );
        let mut codes = vec![
            first.expect("register first concurrently").version_code,
            second.expect("register second concurrently").version_code,
        ];
        codes.sort_unstable();

        assert_eq!(codes, vec![100, 101]);
    }

    #[tokio::test]
    async fn rejects_incomplete_manifest_and_invalid_versions() {
        let db = database().await;
        let fixture = ReleaseFixture::create(&db).await;
        let service = ApplicationReleaseService::new(db);
        let invalid = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "v1.0.0", "a"))
            .await
            .expect_err("prefixed version must fail");
        assert!(matches!(invalid, ApplicationReleaseError::Validation(_)));

        let api = service
            .register_unit_release(fixture.unit_release_input(fixture.api_unit_id, "1.0.0", "a"))
            .await
            .expect("register api");
        let incomplete = service
            .create_application_release(CreateApplicationReleaseInput {
                app_id: fixture.app_id,
                version: "1.0.0".to_owned(),
                base_app_release_id: None,
                unit_changes: vec![UnitReleaseChange::active(
                    fixture.api_unit_id,
                    api.release_id,
                )],
                environment_configs: vec![EnvironmentConfigSelection {
                    environment_id: fixture.environment_id,
                    config_revision_id: fixture.config_revision_id,
                }],
                created_by: "ci".to_owned(),
            })
            .await
            .expect_err("missing unit must fail");
        assert!(matches!(incomplete, ApplicationReleaseError::Validation(_)));
    }

    struct ReleaseFixture {
        app_id: i64,
        api_unit_id: i64,
        web_unit_id: i64,
        environment_id: i64,
        config_revision_id: i64,
    }

    impl ReleaseFixture {
        async fn create(db: &SqlitePool) -> Self {
            Self::create_named(db, "release-app").await
        }

        async fn create_named(db: &SqlitePool, key: &str) -> Self {
            let app_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES (?1, ?1, 'compose', 'compose', '/srv/app', 'ready') RETURNING id",
            )
            .bind(key)
            .fetch_one(db)
            .await
            .expect("insert app");
            let environment_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO app_environments(app_id, environment_key, name, status) VALUES (?1, 'production', '正式环境', 'ready') RETURNING id",
            )
            .bind(app_id)
            .fetch_one(db)
            .await
            .expect("insert environment");
            let api_unit_id = Self::insert_unit(db, app_id, "api", 1).await;
            let web_unit_id = Self::insert_unit(db, app_id, "web", 2).await;
            let config_revision_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO app_config_revisions(app_id, revision_no, config_json, public_config_json, config_hash) VALUES (?1, 100, '{}', '{}', ?2) RETURNING id",
            )
            .bind(app_id)
            .bind(format!("config-{key}"))
            .fetch_one(db)
            .await
            .expect("insert config revision");
            Self {
                app_id,
                api_unit_id,
                web_unit_id,
                environment_id,
                config_revision_id,
            }
        }

        async fn insert_unit(db: &SqlitePool, app_id: i64, key: &str, order: i64) -> i64 {
            let unit_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO deployment_units(app_id, unit_key, name, work_dir) VALUES (?1, ?2, ?2, ?3) RETURNING id",
            )
            .bind(app_id)
            .bind(key)
            .bind(format!("/srv/app/{key}"))
            .fetch_one(db)
            .await
            .expect("insert unit");
            let stage_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO deployment_pipeline_stages(app_id, stage_no, stage_key, name) VALUES (?1, ?2, ?3, ?3) RETURNING id",
            )
            .bind(app_id)
            .bind(order)
            .bind(key)
            .fetch_one(db)
            .await
            .expect("insert stage");
            sqlx::query("INSERT INTO deployment_pipeline_stage_units(stage_id, unit_id, unit_order, removal_order) VALUES (?1, ?2, 1, 1)")
                .bind(stage_id)
                .bind(unit_id)
                .execute(db)
                .await
                .expect("insert stage unit");
            unit_id
        }

        fn unit_release_input(
            &self,
            unit_id: i64,
            version: &str,
            checksum_seed: &str,
        ) -> RegisterUnitReleaseInput {
            RegisterUnitReleaseInput {
                unit_id,
                version: version.to_owned(),
                package_name: format!("unit-{version}.tar.gz"),
                package_path: format!("/tmp/unit-{version}.tar.gz"),
                extract_dir: String::new(),
                checksum_sha256: checksum_seed.repeat(64),
                size_bytes: 42,
                published_at: "2026-07-11T00:00:00Z".to_owned(),
                source: "openapi".to_owned(),
                metadata: serde_json::json!({}),
                storage: UnitReleaseStorage::local(),
            }
        }
    }
}
