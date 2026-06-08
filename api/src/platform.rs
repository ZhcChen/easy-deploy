use sqlx::SqlitePool;

pub const DEFAULT_APP_WORK_DIR_TEMPLATE: &str = "/opt/easy-deploy/apps/{app_key}";
pub const DEFAULT_NODE_WORK_DIR: &str = "/opt/easy-deploy/apps";
pub const DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP: usize = 4;

const APP_WORK_DIR_KEY: &str = "default_app_work_dir";
const NODE_WORK_DIR_KEY: &str = "default_node_work_dir";
const UPLOADED_RELEASES_TO_KEEP_KEY: &str = "uploaded_binary_releases_to_keep";

#[derive(Clone)]
pub struct PlatformConfigService {
    db: SqlitePool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformConfig {
    pub default_app_work_dir: String,
    pub default_node_work_dir: String,
    pub uploaded_binary_releases_to_keep: usize,
}

#[derive(Clone, Debug)]
pub struct UpdatePlatformConfigInput {
    pub default_app_work_dir: String,
    pub default_node_work_dir: String,
    pub uploaded_binary_releases_to_keep: usize,
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

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            default_app_work_dir: DEFAULT_APP_WORK_DIR_TEMPLATE.to_owned(),
            default_node_work_dir: DEFAULT_NODE_WORK_DIR.to_owned(),
            uploaded_binary_releases_to_keep: DEFAULT_UPLOADED_BINARY_RELEASES_TO_KEEP,
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
                _ => {}
            }
        }
        Ok(config)
    }

    pub async fn update_config(
        &self,
        input: UpdatePlatformConfigInput,
        actor: &str,
    ) -> Result<PlatformConfig, PlatformConfigError> {
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
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| PlatformConfigError::InvalidInput("上传制品保留数量必须是数字".to_owned()))?;
    if !(1..=30).contains(&parsed) {
        return Err(PlatformConfigError::InvalidInput(
            "上传制品保留数量必须在 1 到 30 之间".to_owned(),
        ));
    }
    Ok(parsed)
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
    use super::*;

    #[test]
    fn app_work_dir_template_requires_app_key_placeholder() {
        let err = normalize_app_work_dir_template("/opt/easy-deploy/apps")
            .expect_err("missing placeholder should fail");
        assert!(matches!(err, PlatformConfigError::InvalidInput(_)));
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
        assert!(normalize_releases_to_keep("0").is_err());
        assert!(normalize_releases_to_keep("31").is_err());
    }
}
