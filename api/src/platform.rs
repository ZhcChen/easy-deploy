use sqlx::SqlitePool;

use crate::artifact_storage::{
    AliyunOssConfig, ArtifactStorageConfig, ArtifactStorageError,
    DEFAULT_ALIYUN_OSS_DOWNLOAD_TTL_SECONDS, DEFAULT_ALIYUN_OSS_UPLOAD_TTL_SECONDS,
    normalize_storage_provider,
};

pub const DEFAULT_APP_WORK_DIR_TEMPLATE: &str = "/opt/easy-deploy/apps/{app_key}";
pub const DEFAULT_NODE_WORK_DIR: &str = "/opt/easy-deploy/apps";
pub const DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP: usize = 4;

const APP_WORK_DIR_KEY: &str = "default_app_work_dir";
const NODE_WORK_DIR_KEY: &str = "default_node_work_dir";
const UPLOADED_RELEASES_TO_KEEP_KEY: &str = "uploaded_binary_releases_to_keep";
const ARTIFACT_STORAGE_PROVIDER_KEY: &str = "artifact_storage_provider";
const ALIYUN_OSS_REGION_KEY: &str = "aliyun_oss_region";
const ALIYUN_OSS_ENDPOINT_KEY: &str = "aliyun_oss_endpoint";
const ALIYUN_OSS_BUCKET_KEY: &str = "aliyun_oss_bucket";
const ALIYUN_OSS_OBJECT_PREFIX_KEY: &str = "aliyun_oss_object_prefix";
const ALIYUN_OSS_ACCESS_KEY_ID_KEY: &str = "aliyun_oss_access_key_id";
const ALIYUN_OSS_ACCESS_KEY_SECRET_KEY: &str = "aliyun_oss_access_key_secret";
const ALIYUN_OSS_UPLOAD_TTL_KEY: &str = "aliyun_oss_upload_url_ttl_seconds";
const ALIYUN_OSS_DOWNLOAD_TTL_KEY: &str = "aliyun_oss_download_url_ttl_seconds";

#[derive(Clone)]
pub struct PlatformConfigService {
    db: SqlitePool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformConfig {
    pub default_app_work_dir: String,
    pub default_node_work_dir: String,
    pub uploaded_binary_releases_to_keep: usize,
    pub artifact_storage: ArtifactStorageConfig,
}

#[derive(Clone, Debug)]
pub struct UpdatePlatformConfigInput {
    pub default_app_work_dir: String,
    pub default_node_work_dir: String,
    pub uploaded_binary_releases_to_keep: usize,
    pub artifact_storage_provider: String,
    pub aliyun_oss_region: String,
    pub aliyun_oss_endpoint: String,
    pub aliyun_oss_bucket: String,
    pub aliyun_oss_object_prefix: String,
    pub aliyun_oss_access_key_id: String,
    pub aliyun_oss_access_key_secret: String,
    pub aliyun_oss_upload_url_ttl_seconds: i64,
    pub aliyun_oss_download_url_ttl_seconds: i64,
}

#[derive(Debug)]
pub enum PlatformConfigError {
    InvalidInput(String),
    Internal(String),
}

impl PlatformConfigError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Internal(message) => message,
        }
    }
}

impl std::fmt::Display for PlatformConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PlatformConfigError {}

impl From<sqlx::Error> for PlatformConfigError {
    fn from(value: sqlx::Error) -> Self {
        Self::Internal(format!("平台设置数据操作失败: {value}"))
    }
}

impl From<ArtifactStorageError> for PlatformConfigError {
    fn from(value: ArtifactStorageError) -> Self {
        Self::InvalidInput(value.message().to_owned())
    }
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            default_app_work_dir: DEFAULT_APP_WORK_DIR_TEMPLATE.to_owned(),
            default_node_work_dir: DEFAULT_NODE_WORK_DIR.to_owned(),
            uploaded_binary_releases_to_keep: DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP,
            artifact_storage: ArtifactStorageConfig::default(),
        }
    }
}

impl PlatformConfig {
    pub fn default_app_work_dir_for(&self, app_key: &str) -> String {
        render_app_work_dir(&self.default_app_work_dir, app_key)
    }
}

impl PlatformConfigService {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn config(&self) -> Result<PlatformConfig, PlatformConfigError> {
        let rows = sqlx::query_as::<_, PlatformSettingRow>(
            r#"
            SELECT setting_key, setting_value
            FROM platform_settings
            "#,
        )
        .fetch_all(&self.db)
        .await?;

        let mut config = PlatformConfig::default();
        for row in rows {
            match row.setting_key.as_str() {
                APP_WORK_DIR_KEY => {
                    config.default_app_work_dir =
                        normalize_app_work_dir_template(&row.setting_value)?;
                }
                NODE_WORK_DIR_KEY => {
                    config.default_node_work_dir = normalize_work_dir(
                        &row.setting_value,
                        DEFAULT_NODE_WORK_DIR,
                        "默认节点工作目录",
                    )?;
                }
                UPLOADED_RELEASES_TO_KEEP_KEY => {
                    config.uploaded_binary_releases_to_keep =
                        normalize_releases_to_keep(&row.setting_value)?;
                }
                ARTIFACT_STORAGE_PROVIDER_KEY => {
                    config.artifact_storage.provider =
                        normalize_storage_provider(&row.setting_value)?;
                }
                ALIYUN_OSS_REGION_KEY => {
                    config.artifact_storage.aliyun_oss.region = row.setting_value.trim().to_owned();
                }
                ALIYUN_OSS_ENDPOINT_KEY => {
                    config.artifact_storage.aliyun_oss.endpoint =
                        row.setting_value.trim().to_owned();
                }
                ALIYUN_OSS_BUCKET_KEY => {
                    config.artifact_storage.aliyun_oss.bucket = row.setting_value.trim().to_owned();
                }
                ALIYUN_OSS_OBJECT_PREFIX_KEY => {
                    config.artifact_storage.aliyun_oss.object_prefix =
                        row.setting_value.trim().to_owned();
                }
                ALIYUN_OSS_ACCESS_KEY_ID_KEY => {
                    config.artifact_storage.aliyun_oss.access_key_id =
                        row.setting_value.trim().to_owned();
                }
                ALIYUN_OSS_ACCESS_KEY_SECRET_KEY => {
                    config.artifact_storage.aliyun_oss.access_key_secret =
                        row.setting_value.trim().to_owned();
                }
                ALIYUN_OSS_UPLOAD_TTL_KEY => {
                    config.artifact_storage.aliyun_oss.upload_url_ttl_seconds =
                        normalize_i64(&row.setting_value, DEFAULT_ALIYUN_OSS_UPLOAD_TTL_SECONDS)?;
                }
                ALIYUN_OSS_DOWNLOAD_TTL_KEY => {
                    config.artifact_storage.aliyun_oss.download_url_ttl_seconds =
                        normalize_i64(&row.setting_value, DEFAULT_ALIYUN_OSS_DOWNLOAD_TTL_SECONDS)?;
                }
                _ => {}
            }
        }
        config.artifact_storage = config.artifact_storage.normalize()?;
        Ok(config)
    }

    pub async fn update_config(
        &self,
        input: UpdatePlatformConfigInput,
        actor: &str,
    ) -> Result<PlatformConfig, PlatformConfigError> {
        let current = self.config().await.unwrap_or_default();
        let access_key_secret = if input.aliyun_oss_access_key_secret.trim().is_empty() {
            current
                .artifact_storage
                .aliyun_oss
                .access_key_secret
                .clone()
        } else {
            input.aliyun_oss_access_key_secret.trim().to_owned()
        };
        let artifact_storage = ArtifactStorageConfig {
            provider: input.artifact_storage_provider,
            aliyun_oss: AliyunOssConfig {
                region: input.aliyun_oss_region,
                endpoint: input.aliyun_oss_endpoint,
                bucket: input.aliyun_oss_bucket,
                object_prefix: input.aliyun_oss_object_prefix,
                access_key_id: input.aliyun_oss_access_key_id,
                access_key_secret,
                upload_url_ttl_seconds: input.aliyun_oss_upload_url_ttl_seconds,
                download_url_ttl_seconds: input.aliyun_oss_download_url_ttl_seconds,
            },
        }
        .normalize()?;

        let config = PlatformConfig {
            default_app_work_dir: normalize_app_work_dir_template(&input.default_app_work_dir)?,
            default_node_work_dir: normalize_work_dir(
                &input.default_node_work_dir,
                DEFAULT_NODE_WORK_DIR,
                "默认节点工作目录",
            )?,
            uploaded_binary_releases_to_keep: normalize_releases_to_keep(
                &input.uploaded_binary_releases_to_keep.to_string(),
            )?,
            artifact_storage,
        };

        let mut tx = self.db.begin().await?;
        upsert_setting(
            &mut tx,
            APP_WORK_DIR_KEY,
            &config.default_app_work_dir,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            NODE_WORK_DIR_KEY,
            &config.default_node_work_dir,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            UPLOADED_RELEASES_TO_KEEP_KEY,
            &config.uploaded_binary_releases_to_keep.to_string(),
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ARTIFACT_STORAGE_PROVIDER_KEY,
            &config.artifact_storage.provider,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_REGION_KEY,
            &config.artifact_storage.aliyun_oss.region,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_ENDPOINT_KEY,
            &config.artifact_storage.aliyun_oss.endpoint,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_BUCKET_KEY,
            &config.artifact_storage.aliyun_oss.bucket,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_OBJECT_PREFIX_KEY,
            &config.artifact_storage.aliyun_oss.object_prefix,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_ACCESS_KEY_ID_KEY,
            &config.artifact_storage.aliyun_oss.access_key_id,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_ACCESS_KEY_SECRET_KEY,
            &config.artifact_storage.aliyun_oss.access_key_secret,
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_UPLOAD_TTL_KEY,
            &config
                .artifact_storage
                .aliyun_oss
                .upload_url_ttl_seconds
                .to_string(),
            actor,
        )
        .await?;
        upsert_setting(
            &mut tx,
            ALIYUN_OSS_DOWNLOAD_TTL_KEY,
            &config
                .artifact_storage
                .aliyun_oss
                .download_url_ttl_seconds
                .to_string(),
            actor,
        )
        .await?;
        tx.commit().await?;

        Ok(config)
    }
}

#[derive(sqlx::FromRow)]
struct PlatformSettingRow {
    setting_key: String,
    setting_value: String,
}

async fn upsert_setting(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &str,
    value: &str,
    actor: &str,
) -> Result<(), PlatformConfigError> {
    sqlx::query(
        r#"
        INSERT INTO platform_settings(setting_key, setting_value, updated_by)
        VALUES (?1, ?2, ?3)
        ON CONFLICT(setting_key) DO UPDATE SET
            setting_value = excluded.setting_value,
            updated_by = excluded.updated_by,
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
        "#,
    )
    .bind(key)
    .bind(value)
    .bind(actor)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn normalize_app_work_dir_template(value: &str) -> Result<String, PlatformConfigError> {
    let normalized = normalize_work_dir(value, DEFAULT_APP_WORK_DIR_TEMPLATE, "默认应用部署目录")?;
    if !normalized.contains("{app_key}") {
        return Err(PlatformConfigError::InvalidInput(
            "默认应用部署目录必须包含 {app_key} 占位符".to_owned(),
        ));
    }
    if normalized.rsplit('/').next() != Some("{app_key}") {
        return Err(PlatformConfigError::InvalidInput(
            "默认应用部署目录必须以 {app_key} 作为最后一级目录".to_owned(),
        ));
    }
    Ok(normalized)
}

fn normalize_work_dir(
    value: &str,
    fallback: &str,
    label: &str,
) -> Result<String, PlatformConfigError> {
    let normalized = value
        .trim()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    let normalized = if normalized.is_empty() {
        fallback.to_owned()
    } else {
        normalized
    };
    if normalized.contains('\n') || normalized.contains('\r') {
        return Err(PlatformConfigError::InvalidInput(format!(
            "{label}不能包含换行"
        )));
    }
    if !normalized.starts_with('/') && !normalized.starts_with('.') {
        return Err(PlatformConfigError::InvalidInput(format!(
            "{label}必须使用绝对路径，或使用 . 开头的相对路径"
        )));
    }
    Ok(normalized)
}

fn normalize_releases_to_keep(value: &str) -> Result<usize, PlatformConfigError> {
    let parsed = value.trim().parse::<usize>().map_err(|_| {
        PlatformConfigError::InvalidInput("上传版本包保留数量必须是数字".to_owned())
    })?;
    if !(1..=30).contains(&parsed) {
        return Err(PlatformConfigError::InvalidInput(
            "上传版本包保留数量必须在 1 到 30 之间".to_owned(),
        ));
    }
    Ok(parsed)
}

fn normalize_i64(value: &str, fallback: i64) -> Result<i64, PlatformConfigError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(fallback);
    }
    value
        .parse::<i64>()
        .map_err(|_| PlatformConfigError::InvalidInput("平台设置数值必须是数字".to_owned()))
}

fn render_app_work_dir(template: &str, app_key: &str) -> String {
    let app_key = normalize_app_key_segment(app_key);
    template.replace("{app_key}", &app_key)
}

fn normalize_app_key_segment(value: &str) -> String {
    let mut output = String::new();
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            output.push(ch.to_ascii_lowercase());
        } else if !output.ends_with('-') {
            output.push('-');
        }
    }
    let output = output.trim_matches('-');
    if output.is_empty() {
        "app".to_owned()
    } else {
        output.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqliteConnectOptions;

    use super::*;

    async fn platform_service() -> PlatformConfigService {
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
        PlatformConfigService::new(db)
    }

    fn update_input() -> UpdatePlatformConfigInput {
        UpdatePlatformConfigInput {
            default_app_work_dir: DEFAULT_APP_WORK_DIR_TEMPLATE.to_owned(),
            default_node_work_dir: DEFAULT_NODE_WORK_DIR.to_owned(),
            uploaded_binary_releases_to_keep: DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP,
            artifact_storage_provider: "local".to_owned(),
            aliyun_oss_region: "oss-cn-hangzhou".to_owned(),
            aliyun_oss_endpoint: "https://oss-cn-hangzhou.aliyuncs.com".to_owned(),
            aliyun_oss_bucket: String::new(),
            aliyun_oss_object_prefix: "easy-deploy/releases".to_owned(),
            aliyun_oss_access_key_id: String::new(),
            aliyun_oss_access_key_secret: String::new(),
            aliyun_oss_upload_url_ttl_seconds: 900,
            aliyun_oss_download_url_ttl_seconds: 600,
        }
    }

    #[tokio::test]
    async fn service_reads_defaults_and_persists_updates() {
        let service = platform_service().await;

        let defaults = service.config().await.expect("default config");
        assert_eq!(defaults, PlatformConfig::default());

        let updated = service
            .update_config(
                UpdatePlatformConfigInput {
                    default_app_work_dir: r" \srv\apps\{app_key}\ ".to_owned(),
                    default_node_work_dir: r" .\nodes\ ".to_owned(),
                    uploaded_binary_releases_to_keep: 6,
                    ..update_input()
                },
                "admin",
            )
            .await
            .expect("update config");
        assert_eq!(updated.default_app_work_dir, "/srv/apps/{app_key}");
        assert_eq!(updated.default_node_work_dir, "./nodes");
        assert_eq!(updated.uploaded_binary_releases_to_keep, 6);

        let loaded = service.config().await.expect("load updated config");
        assert_eq!(loaded, updated);
    }

    #[tokio::test]
    async fn service_ignores_unknown_setting_keys() {
        let service = platform_service().await;
        sqlx::query(
            r#"
            INSERT INTO platform_settings(setting_key, setting_value, updated_by)
            VALUES ('unknown_setting', 'ignored', 'test')
            "#,
        )
        .execute(&service.db)
        .await
        .expect("insert unknown setting");

        let config = service.config().await.expect("load config");
        assert_eq!(config, PlatformConfig::default());
    }

    #[test]
    fn platform_config_error_wraps_sqlx_errors() {
        let err = PlatformConfigError::from(sqlx::Error::RowNotFound);
        assert!(matches!(err, PlatformConfigError::Internal(_)));
        assert!(!err.message().trim().is_empty());
    }

    #[tokio::test]
    async fn service_rejects_invalid_updates_before_persisting() {
        let service = platform_service().await;

        let err = service
            .update_config(
                UpdatePlatformConfigInput {
                    default_app_work_dir: "/srv/apps".to_owned(),
                    default_node_work_dir: "/srv/nodes".to_owned(),
                    uploaded_binary_releases_to_keep: 4,
                    ..update_input()
                },
                "admin",
            )
            .await
            .expect_err("missing app key placeholder");

        assert!(matches!(err, PlatformConfigError::InvalidInput(_)));
        assert!(!err.message().is_empty());
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn app_work_dir_template_requires_app_key_placeholder() {
        let err = normalize_app_work_dir_template("/opt/easy-deploy/apps")
            .expect_err("missing placeholder should fail");
        assert!(matches!(err, PlatformConfigError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn service_persists_artifact_storage_and_preserves_blank_secret() {
        let service = platform_service().await;

        let updated = service
            .update_config(
                UpdatePlatformConfigInput {
                    artifact_storage_provider: "aliyun_oss".to_owned(),
                    aliyun_oss_bucket: "easy-deploy-artifacts".to_owned(),
                    aliyun_oss_access_key_id: "ak".to_owned(),
                    aliyun_oss_access_key_secret: "secret-a".to_owned(),
                    aliyun_oss_upload_url_ttl_seconds: 120,
                    aliyun_oss_download_url_ttl_seconds: 180,
                    ..update_input()
                },
                "admin",
            )
            .await
            .expect("save oss config");
        assert_eq!(updated.artifact_storage.provider, "aliyun_oss");
        assert_eq!(
            updated.artifact_storage.aliyun_oss.bucket,
            "easy-deploy-artifacts"
        );
        assert_eq!(
            updated.artifact_storage.aliyun_oss.access_key_secret,
            "secret-a"
        );

        let preserved = service
            .update_config(
                UpdatePlatformConfigInput {
                    artifact_storage_provider: "aliyun_oss".to_owned(),
                    aliyun_oss_bucket: "easy-deploy-artifacts".to_owned(),
                    aliyun_oss_access_key_id: "ak2".to_owned(),
                    aliyun_oss_access_key_secret: String::new(),
                    ..update_input()
                },
                "admin",
            )
            .await
            .expect("preserve secret");
        assert_eq!(
            preserved.artifact_storage.aliyun_oss.access_key_secret,
            "secret-a"
        );
        assert_eq!(preserved.artifact_storage.aliyun_oss.access_key_id, "ak2");
    }

    #[test]
    fn app_work_dir_template_requires_app_key_as_last_segment() {
        let err = normalize_app_work_dir_template("/opt/easy-deploy/{app_key}/apps")
            .expect_err("placeholder must be final segment");
        assert!(matches!(err, PlatformConfigError::InvalidInput(_)));
        assert!(err.message().contains("最后一级目录"));
    }

    #[test]
    fn app_work_dir_template_renders_normalized_app_key() {
        let config = PlatformConfig {
            default_app_work_dir: "/srv/deploy/{app_key}".to_owned(),
            ..PlatformConfig::default()
        };

        assert_eq!(
            config.default_app_work_dir_for("Orders API"),
            "/srv/deploy/orders-api"
        );
    }

    #[test]
    fn releases_to_keep_has_practical_bounds() {
        assert_eq!(normalize_releases_to_keep("4").expect("valid value"), 4);
        assert!(normalize_releases_to_keep("many").is_err());
        assert!(normalize_releases_to_keep("0").is_err());
        assert!(normalize_releases_to_keep("31").is_err());
    }

    #[test]
    fn work_dir_and_app_key_normalizers_cover_defaults_and_errors() {
        assert_eq!(
            normalize_work_dir("", "/opt/easy-deploy/apps", "path").expect("fallback"),
            "/opt/easy-deploy/apps"
        );
        assert_eq!(
            normalize_work_dir(r".\runtime\", "/fallback", "path").expect("relative path"),
            "./runtime"
        );
        assert!(normalize_work_dir("relative", "/fallback", "path").is_err());
        assert!(normalize_work_dir("/srv/apps\nbad", "/fallback", "path").is_err());

        assert_eq!(
            normalize_app_key_segment(" Orders API ++ Prod "),
            "orders-api-prod"
        );
        assert_eq!(normalize_app_key_segment("###"), "app");
    }
}
