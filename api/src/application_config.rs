use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::secret_config::{SecretConfigCipher, SecretConfigError};

#[derive(Clone)]
pub struct ApplicationConfigService {
    db: SqlitePool,
    cipher: SecretConfigCipher,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationConfigError {
    Validation(String),
    Conflict(String),
    NotFound(String),
    Secret(String),
    Database(String),
}

impl std::fmt::Display for ApplicationConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message)
            | Self::Conflict(message)
            | Self::NotFound(message)
            | Self::Secret(message)
            | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ApplicationConfigError {}

impl From<sqlx::Error> for ApplicationConfigError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<SecretConfigError> for ApplicationConfigError {
    fn from(error: SecretConfigError) -> Self {
        Self::Secret(error.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationConfigDocument {
    pub environments: Vec<ConfigEnvironment>,
    pub units: Vec<ConfigUnit>,
    pub stages: Vec<ConfigStage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigEnvironment {
    pub key: String,
    pub name: String,
    #[serde(default = "default_parallel_units")]
    pub max_parallel_units: u8,
    #[serde(default)]
    pub target_node_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigUnit {
    pub key: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default = "default_active_status")]
    pub status: String,
    pub work_dir: String,
    #[serde(default)]
    pub compose_content: String,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    #[serde(default)]
    pub health_check: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigStage {
    pub number: u16,
    pub key: String,
    pub name: String,
    #[serde(default = "default_stage_kind")]
    pub kind: String,
    #[serde(default)]
    pub unit_keys: Vec<String>,
    #[serde(default)]
    pub check_config: JsonValue,
}

#[derive(Debug, Clone)]
pub struct SaveConfigDraftInput {
    pub app_id: i64,
    pub document: ApplicationConfigDocument,
    pub secret_values: BTreeMap<String, String>,
    pub updated_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDraftResult {
    pub draft_id: i64,
    pub draft_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRevisionResult {
    pub revision_id: i64,
    pub revision_no: i64,
    pub config_hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PublishedApplicationConfig {
    pub revision_id: i64,
    pub revision_no: i64,
    pub config_hash: String,
    pub document: ApplicationConfigDocument,
    pub secret_values: BTreeMap<String, String>,
}

#[derive(Debug, sqlx::FromRow)]
struct ConfigRevisionRow {
    id: i64,
    revision_no: i64,
    config_json: String,
    secret_ciphertext: String,
    config_hash: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ConfigDraftRow {
    id: i64,
    draft_json: String,
    draft_hash: String,
    secret_ciphertext: String,
    secret_fingerprints: String,
    encryption_key_id: String,
}

impl ApplicationConfigService {
    pub fn new(db: SqlitePool, cipher: SecretConfigCipher) -> Self {
        Self { db, cipher }
    }

    pub async fn load_revision(
        &self,
        app_id: i64,
        revision_id: i64,
    ) -> Result<PublishedApplicationConfig, ApplicationConfigError> {
        let revision = sqlx::query_as::<_, ConfigRevisionRow>(
            r#"
            SELECT id, revision_no, config_json, secret_ciphertext, config_hash
            FROM app_config_revisions
            WHERE id = ?1 AND app_id = ?2
            "#,
        )
        .bind(revision_id)
        .bind(app_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| ApplicationConfigError::NotFound("config revision not found".to_owned()))?;
        let document = serde_json::from_str::<ApplicationConfigDocument>(&revision.config_json)
            .map_err(|error| ApplicationConfigError::Validation(error.to_string()))?;
        validate_document(&document)?;
        let secret_json = self.cipher.decrypt(&revision.secret_ciphertext)?;
        let secret_values = serde_json::from_slice::<BTreeMap<String, String>>(&secret_json)
            .map_err(|error| ApplicationConfigError::Secret(error.to_string()))?;
        Ok(PublishedApplicationConfig {
            revision_id: revision.id,
            revision_no: revision.revision_no,
            config_hash: revision.config_hash,
            document,
            secret_values,
        })
    }

    pub async fn save_draft(
        &self,
        input: SaveConfigDraftInput,
    ) -> Result<ConfigDraftResult, ApplicationConfigError> {
        validate_document(&input.document)?;
        let public_json = canonical_json(&input.document)?;
        let secret_json = serde_json::to_vec(&input.secret_values)
            .map_err(|error| ApplicationConfigError::Validation(error.to_string()))?;
        let secret_ciphertext = self.cipher.encrypt(&secret_json)?;
        let secret_fingerprints = secret_fingerprints(&input.secret_values);
        let secret_fingerprints_json = serde_json::to_string(&secret_fingerprints)
            .map_err(|error| ApplicationConfigError::Validation(error.to_string()))?;
        let draft_hash =
            content_hash(&[public_json.as_bytes(), secret_fingerprints_json.as_bytes()]);
        sqlx::query(
            r#"
            INSERT INTO app_config_drafts(
                app_id, draft_json, draft_hash, updated_by,
                secret_ciphertext, secret_fingerprints, encryption_key_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(app_id) DO UPDATE SET
                draft_json = excluded.draft_json,
                draft_hash = excluded.draft_hash,
                updated_by = excluded.updated_by,
                secret_ciphertext = excluded.secret_ciphertext,
                secret_fingerprints = excluded.secret_fingerprints,
                encryption_key_id = excluded.encryption_key_id,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(input.app_id)
        .bind(public_json)
        .bind(&draft_hash)
        .bind(input.updated_by)
        .bind(secret_ciphertext)
        .bind(secret_fingerprints_json)
        .bind(self.cipher.active_key_id())
        .execute(&self.db)
        .await?;
        let draft_id = sqlx::query_scalar("SELECT id FROM app_config_drafts WHERE app_id = ?1")
            .bind(input.app_id)
            .fetch_one(&self.db)
            .await?;
        Ok(ConfigDraftResult {
            draft_id,
            draft_hash,
        })
    }

    pub async fn publish_draft(
        &self,
        app_id: i64,
        expected_draft_hash: &str,
        published_by: &str,
    ) -> Result<ConfigRevisionResult, ApplicationConfigError> {
        let mut tx = self.db.begin().await?;
        let draft = sqlx::query_as::<_, ConfigDraftRow>(
            r#"
            SELECT id, draft_json, draft_hash, secret_ciphertext,
                   secret_fingerprints, encryption_key_id
            FROM app_config_drafts
            WHERE app_id = ?1
            "#,
        )
        .bind(app_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApplicationConfigError::NotFound("config draft not found".to_owned()))?;
        if draft.draft_hash != expected_draft_hash {
            return Err(ApplicationConfigError::Conflict(
                "config draft changed; reload before publishing".to_owned(),
            ));
        }
        let document: ApplicationConfigDocument = serde_json::from_str(&draft.draft_json)
            .map_err(|error| ApplicationConfigError::Validation(error.to_string()))?;
        validate_document(&document)?;
        self.cipher.decrypt(&draft.secret_ciphertext)?;

        if let Some(existing) = sqlx::query_as::<_, (i64, i64, String)>(
            "SELECT id, revision_no, config_hash FROM app_config_revisions WHERE app_id = ?1 AND config_hash = ?2",
        )
        .bind(app_id)
        .bind(&draft.draft_hash)
        .fetch_optional(&mut *tx)
        .await?
        {
            sqlx::query("DELETE FROM app_config_drafts WHERE id = ?1")
                .bind(draft.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(ConfigRevisionResult {
                revision_id: existing.0,
                revision_no: existing.1,
                config_hash: existing.2,
            });
        }

        sqlx::query(
            r#"
            INSERT INTO version_counters(scope_kind, scope_id, next_value)
            VALUES ('config_revision', ?1, 100)
            ON CONFLICT(scope_kind, scope_id) DO NOTHING
            "#,
        )
        .bind(app_id)
        .execute(&mut *tx)
        .await?;
        let revision_no: i64 = sqlx::query_scalar(
            r#"
            UPDATE version_counters
            SET next_value = next_value + 1,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE scope_kind = 'config_revision' AND scope_id = ?1
            RETURNING next_value - 1
            "#,
        )
        .bind(app_id)
        .fetch_one(&mut *tx)
        .await?;
        let script_hash = script_hash(&document);
        let revision_id = sqlx::query(
            r#"
            INSERT INTO app_config_revisions(
                app_id, revision_no, config_json, public_config_json,
                secret_ciphertext, secret_fingerprints, config_hash,
                script_hash, encryption_key_id, published_by
            )
            VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
        )
        .bind(app_id)
        .bind(revision_no)
        .bind(&draft.draft_json)
        .bind(&draft.secret_ciphertext)
        .bind(&draft.secret_fingerprints)
        .bind(&draft.draft_hash)
        .bind(script_hash)
        .bind(&draft.encryption_key_id)
        .bind(published_by.trim())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query("DELETE FROM app_config_drafts WHERE id = ?1")
            .bind(draft.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ConfigRevisionResult {
            revision_id,
            revision_no,
            config_hash: draft.draft_hash,
        })
    }
}

fn validate_document(document: &ApplicationConfigDocument) -> Result<(), ApplicationConfigError> {
    if document.environments.is_empty() {
        return Err(validation("at least one environment is required"));
    }
    if document.units.is_empty() {
        return Err(validation("at least one deployment unit is required"));
    }
    if document.stages.is_empty() {
        return Err(validation("at least one pipeline stage is required"));
    }

    let mut environment_keys = BTreeSet::new();
    for environment in &document.environments {
        validate_key("environment", &environment.key)?;
        if !environment_keys.insert(environment.key.as_str()) {
            return Err(validation("environment keys must be unique"));
        }
        if environment.name.trim().is_empty() {
            return Err(validation("environment name is required"));
        }
        if !(1..=32).contains(&environment.max_parallel_units) {
            return Err(validation("environment parallel unit limit must be 1..=32"));
        }
        if environment.target_node_ids.is_empty() {
            return Err(validation("environment must have at least one target node"));
        }
    }

    let mut unit_keys = BTreeSet::new();
    let mut required_active_units = BTreeSet::new();
    for unit in &document.units {
        validate_key("deployment unit", &unit.key)?;
        if !unit_keys.insert(unit.key.as_str()) {
            return Err(validation("deployment unit keys must be unique"));
        }
        if unit.name.trim().is_empty() {
            return Err(validation("deployment unit name is required"));
        }
        if !matches!(unit.status.as_str(), "active" | "disabled") {
            return Err(validation(
                "deployment unit status must be active or disabled",
            ));
        }
        if !unit.work_dir.starts_with('/') || unit.work_dir.contains("..") {
            return Err(validation(
                "deployment unit work directory must be an absolute normalized path",
            ));
        }
        if unit.required && unit.status == "active" {
            required_active_units.insert(unit.key.as_str());
        }
    }

    let mut stage_numbers = BTreeSet::new();
    let mut stage_keys = BTreeSet::new();
    let mut staged_units = BTreeSet::new();
    for stage in &document.stages {
        if stage.number == 0 || !stage_numbers.insert(stage.number) {
            return Err(validation(
                "pipeline stage numbers must be unique and positive",
            ));
        }
        validate_key("pipeline stage", &stage.key)?;
        if !stage_keys.insert(stage.key.as_str()) {
            return Err(validation("pipeline stage keys must be unique"));
        }
        match stage.kind.as_str() {
            "units" => {
                if stage.unit_keys.is_empty() {
                    return Err(validation("unit stage must include at least one unit"));
                }
                for unit_key in &stage.unit_keys {
                    if !unit_keys.contains(unit_key.as_str()) {
                        return Err(validation("pipeline stage references an unknown unit"));
                    }
                    if !staged_units.insert(unit_key.as_str()) {
                        return Err(validation("deployment unit can appear in only one stage"));
                    }
                }
            }
            "application_check" => {
                if !stage.unit_keys.is_empty() {
                    return Err(validation("application check stage cannot reference units"));
                }
            }
            _ => return Err(validation("unsupported pipeline stage kind")),
        }
    }
    if !required_active_units.is_subset(&staged_units) {
        return Err(validation(
            "every required active deployment unit must belong to a pipeline stage",
        ));
    }
    Ok(())
}

fn validate_key(kind: &str, value: &str) -> Result<(), ApplicationConfigError> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
        });
    if valid {
        Ok(())
    } else {
        Err(validation(&format!(
            "{kind} key must contain lowercase letters, digits, dash or underscore"
        )))
    }
}

fn canonical_json(document: &ApplicationConfigDocument) -> Result<String, ApplicationConfigError> {
    serde_json::to_string(document)
        .map_err(|error| ApplicationConfigError::Validation(error.to_string()))
}

fn secret_fingerprints(values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| (key.clone(), content_hash(&[value.as_bytes()])))
        .collect()
}

fn script_hash(document: &ApplicationConfigDocument) -> String {
    let scripts = document
        .units
        .iter()
        .flat_map(|unit| {
            unit.scripts
                .iter()
                .map(move |(slot, script)| format!("{}:{slot}:{script}", unit.key))
        })
        .collect::<Vec<_>>()
        .join("\n");
    content_hash(&[scripts.as_bytes()])
}

fn content_hash(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

fn validation(message: &str) -> ApplicationConfigError {
    ApplicationConfigError::Validation(message.to_owned())
}

const fn default_parallel_units() -> u8 {
    3
}

const fn default_true() -> bool {
    true
}

fn default_active_status() -> String {
    "active".to_owned()
}

fn default_stage_kind() -> String {
    "units".to_owned()
}

#[cfg(test)]
mod tests {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sqlx::sqlite::SqliteConnectOptions;

    use super::*;

    fn valid_document() -> ApplicationConfigDocument {
        ApplicationConfigDocument {
            environments: vec![ConfigEnvironment {
                key: "testing".to_owned(),
                name: "测试环境".to_owned(),
                max_parallel_units: 3,
                target_node_ids: vec![1],
            }],
            units: vec![ConfigUnit {
                key: "api".to_owned(),
                name: "API".to_owned(),
                required: true,
                status: "active".to_owned(),
                work_dir: "/srv/example/api".to_owned(),
                compose_content: "services: {}".to_owned(),
                scripts: BTreeMap::from([("deploy".to_owned(), "docker compose up -d".to_owned())]),
                health_check: serde_json::json!({"kind": "http", "endpoint": "http://127.0.0.1/healthz"}),
            }],
            stages: vec![ConfigStage {
                number: 1,
                key: "api".to_owned(),
                name: "API".to_owned(),
                kind: "units".to_owned(),
                unit_keys: vec!["api".to_owned()],
                check_config: serde_json::json!({}),
            }],
        }
    }

    fn cipher() -> SecretConfigCipher {
        SecretConfigCipher::from_base64_keys(
            "v1",
            &BTreeMap::from([("v1".to_owned(), STANDARD.encode([8_u8; 32]))]),
        )
        .expect("create cipher")
    }

    async fn service() -> (ApplicationConfigService, SqlitePool, i64) {
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
        let app_id: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status)
            VALUES ('config-app', 'Config App', 'compose', 'compose', '/srv/config-app', 'ready')
            RETURNING id
            "#,
        )
        .fetch_one(&db)
        .await
        .expect("insert app");
        (
            ApplicationConfigService::new(db.clone(), cipher()),
            db,
            app_id,
        )
    }

    #[test]
    fn validates_stage_references_and_required_units() {
        let mut document = valid_document();
        document.stages[0].unit_keys = vec!["missing".to_owned()];
        assert!(validate_document(&document).is_err());

        document.stages[0].unit_keys.clear();
        assert!(validate_document(&document).is_err());
    }

    #[test]
    fn validates_environment_and_work_dir_boundaries() {
        let mut document = valid_document();
        document.environments[0].target_node_ids.clear();
        assert!(validate_document(&document).is_err());

        document = valid_document();
        document.units[0].work_dir = "../escape".to_owned();
        assert!(validate_document(&document).is_err());
    }

    #[tokio::test]
    async fn saves_encrypted_draft_and_publishes_revision_from_100() {
        let (service, db, app_id) = service().await;
        let draft = service
            .save_draft(SaveConfigDraftInput {
                app_id,
                document: valid_document(),
                secret_values: BTreeMap::from([(
                    "testing.api.APP_SECRET".to_owned(),
                    "top-secret".to_owned(),
                )]),
                updated_by: "admin".to_owned(),
            })
            .await
            .expect("save draft");
        let stored: (String, String) = sqlx::query_as(
            "SELECT draft_json, secret_ciphertext FROM app_config_drafts WHERE app_id = ?1",
        )
        .bind(app_id)
        .fetch_one(&db)
        .await
        .expect("load draft");
        assert!(!stored.0.contains("top-secret"));
        assert!(!stored.1.contains("top-secret"));

        let revision = service
            .publish_draft(app_id, &draft.draft_hash, "operator")
            .await
            .expect("publish draft");

        assert_eq!(revision.revision_no, 100);
        let persisted: (String, String, String) = sqlx::query_as(
            r#"
            SELECT public_config_json, secret_ciphertext, published_by
            FROM app_config_revisions
            WHERE id = ?1
            "#,
        )
        .bind(revision.revision_id)
        .fetch_one(&db)
        .await
        .expect("load revision");
        assert!(!persisted.0.contains("top-secret"));
        assert!(!persisted.1.contains("top-secret"));
        assert_eq!(persisted.2, "operator");

        let loaded = service
            .load_revision(app_id, revision.revision_id)
            .await
            .expect("load published revision");
        assert_eq!(loaded.revision_no, 100);
        assert_eq!(loaded.document, valid_document());
        assert_eq!(
            loaded.secret_values.get("testing.api.APP_SECRET"),
            Some(&"top-secret".to_owned())
        );

        let other_app_id: i64 = sqlx::query_scalar(
            "INSERT INTO apps(app_key, name, app_type, deploy_mode, work_dir, status) VALUES ('other-config-app', 'Other', 'compose', 'compose', '/srv/other', 'ready') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .expect("insert other app");
        assert!(
            matches!(
                service
                    .load_revision(other_app_id, revision.revision_id)
                    .await,
                Err(ApplicationConfigError::NotFound(_))
            ),
            "config revisions must not be readable through another application"
        );
    }

    #[tokio::test]
    async fn publishing_detects_changed_draft() {
        let (service, _db, app_id) = service().await;
        let first = service
            .save_draft(SaveConfigDraftInput {
                app_id,
                document: valid_document(),
                secret_values: BTreeMap::new(),
                updated_by: "admin".to_owned(),
            })
            .await
            .expect("save first draft");
        let mut changed = valid_document();
        changed.units[0].name = "Changed API".to_owned();
        service
            .save_draft(SaveConfigDraftInput {
                app_id,
                document: changed,
                secret_values: BTreeMap::new(),
                updated_by: "admin".to_owned(),
            })
            .await
            .expect("save changed draft");

        let error = service
            .publish_draft(app_id, &first.draft_hash, "admin")
            .await
            .expect_err("stale draft hash must fail");
        assert!(matches!(error, ApplicationConfigError::Conflict(_)));
    }

    #[tokio::test]
    async fn publishing_identical_config_reuses_revision_and_clears_draft() {
        let (service, db, app_id) = service().await;
        let save = || SaveConfigDraftInput {
            app_id,
            document: valid_document(),
            secret_values: BTreeMap::from([(
                "testing.api.APP_SECRET".to_owned(),
                "top-secret".to_owned(),
            )]),
            updated_by: "admin".to_owned(),
        };
        let first_draft = service.save_draft(save()).await.expect("save first draft");
        let first_revision = service
            .publish_draft(app_id, &first_draft.draft_hash, "admin")
            .await
            .expect("publish first revision");

        let repeated_draft = service
            .save_draft(save())
            .await
            .expect("save identical draft");
        let repeated_revision = service
            .publish_draft(app_id, &repeated_draft.draft_hash, "admin")
            .await
            .expect("reuse existing revision");

        assert_eq!(repeated_revision, first_revision);
        let remaining_drafts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM app_config_drafts WHERE app_id = ?1")
                .bind(app_id)
                .fetch_one(&db)
                .await
                .expect("count drafts");
        assert_eq!(remaining_drafts, 0);
    }
}
