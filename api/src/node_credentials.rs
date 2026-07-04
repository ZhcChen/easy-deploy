use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::{fs, process::Command};

#[derive(Clone)]
pub struct NodeCredentialService {
    db: SqlitePool,
    data_dir: PathBuf,
}

#[derive(Debug)]
pub enum NodeCredentialError {
    InvalidInput(String),
    Conflict(String),
    Internal(String),
}

impl NodeCredentialError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message) | Self::Conflict(message) | Self::Internal(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for NodeCredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for NodeCredentialError {}

impl From<sqlx::Error> for NodeCredentialError {
    fn from(value: sqlx::Error) -> Self {
        if let sqlx::Error::Database(err) = &value
            && err.is_unique_violation()
        {
            return Self::Conflict("节点凭据标识已存在".to_owned());
        }
        Self::Internal(format!("节点凭据数据操作失败: {value}"))
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct NodeCredentialListItem {
    pub id: i64,
    pub credential_key: String,
    pub name: String,
    pub public_key: String,
    pub private_key_path: String,
    pub fingerprint: String,
    pub passphrase_hint: String,
    pub status: String,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub bound_node_count: i64,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct NodeCredentialOption {
    pub id: i64,
    pub name: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct CreateGeneratedCredentialInput {
    pub name: String,
    pub key_algorithm: String,
    pub created_by: String,
}

#[derive(Clone, Debug)]
pub struct CreateUploadedCredentialInput {
    pub name: String,
    pub private_key: String,
    pub public_key: String,
    pub passphrase_hint: String,
    pub created_by: String,
}

#[derive(Clone, Debug)]
pub struct NodeCredentialCreated {
    pub id: i64,
    pub name: String,
    pub public_key: String,
    pub fingerprint: String,
}

impl NodeCredentialService {
    pub fn new(db: SqlitePool, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            db,
            data_dir: data_dir.into(),
        }
    }

    pub async fn list_credentials(
        &self,
    ) -> Result<Vec<NodeCredentialListItem>, NodeCredentialError> {
        sqlx::query_as::<_, NodeCredentialListItem>(
            r#"
            SELECT
                c.id,
                c.credential_key,
                c.name,
                c.public_key,
                c.private_key_path,
                c.fingerprint,
                c.passphrase_hint,
                c.status,
                c.created_by,
                c.created_at,
                c.updated_at,
                COUNT(n.id) AS bound_node_count
            FROM node_credentials c
            LEFT JOIN nodes n ON n.credential_id = c.id
            GROUP BY c.id
            ORDER BY
                CASE c.status WHEN 'active' THEN 0 ELSE 1 END,
                c.id DESC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(NodeCredentialError::from)
    }

    pub async fn active_options(&self) -> Result<Vec<NodeCredentialOption>, NodeCredentialError> {
        sqlx::query_as::<_, NodeCredentialOption>(
            r#"
            SELECT id, name, fingerprint
            FROM node_credentials
            WHERE status = 'active'
            ORDER BY id DESC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(NodeCredentialError::from)
    }

    pub async fn create_generated_key(
        &self,
        input: CreateGeneratedCredentialInput,
    ) -> Result<NodeCredentialCreated, NodeCredentialError> {
        let name = required_text(&input.name, "请输入凭据名称")?;
        let key_algorithm = GeneratedKeyAlgorithm::parse(&input.key_algorithm)?;
        let created_by = input.created_by.trim().to_owned();
        let credential_key = generated_credential_key();
        let credential_dir = self.credential_dir(&credential_key)?;
        let private_key_path = credential_dir.join(key_algorithm.private_key_file());
        let public_key_path =
            credential_dir.join(format!("{}.pub", key_algorithm.private_key_file()));

        fs::create_dir_all(&credential_dir)
            .await
            .map_err(|err| io_error("创建凭据目录", &credential_dir, err))?;

        let mut command = Command::new("ssh-keygen");
        command.arg("-t").arg(key_algorithm.ssh_keygen_type());
        if let Some(bits) = key_algorithm.bits() {
            command.arg("-b").arg(bits.to_string());
        }
        let output = command
            .arg("-N")
            .arg("")
            .arg("-C")
            .arg(format!("easy-deploy:{credential_key}"))
            .arg("-f")
            .arg(&private_key_path)
            .output()
            .await
            .map_err(|err| NodeCredentialError::Internal(format!("执行 ssh-keygen 失败: {err}")))?;
        if !output.status.success() {
            return Err(NodeCredentialError::Internal(format!(
                "生成 SSH 密钥失败: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        restrict_private_key(&private_key_path).await?;

        let public_key = fs::read_to_string(&public_key_path)
            .await
            .map_err(|err| io_error("读取生成的公钥", &public_key_path, err))?
            .trim()
            .to_owned();
        validate_public_key(&public_key)?;
        let fingerprint = public_key_fingerprint(&public_key);

        let id = insert_credential(
            &self.db,
            InsertCredential {
                credential_key: &credential_key,
                name: &name,
                public_key: &public_key,
                private_key_path: &private_key_path.to_string_lossy(),
                fingerprint: &fingerprint,
                passphrase_hint: "",
                created_by: &created_by,
            },
        )
        .await?;

        Ok(NodeCredentialCreated {
            id,
            name,
            public_key,
            fingerprint,
        })
    }

    pub async fn create_uploaded_key(
        &self,
        input: CreateUploadedCredentialInput,
    ) -> Result<NodeCredentialCreated, NodeCredentialError> {
        let name = required_text(&input.name, "请输入凭据名称")?;
        let private_key = normalize_private_key(&input.private_key)?;
        let created_by = input.created_by.trim().to_owned();
        let passphrase_hint = input.passphrase_hint.trim().to_owned();
        let credential_key = generated_credential_key();
        let credential_dir = self.credential_dir(&credential_key)?;
        let private_key_path = credential_dir.join("id_ed25519");

        fs::create_dir_all(&credential_dir)
            .await
            .map_err(|err| io_error("创建凭据目录", &credential_dir, err))?;
        fs::write(&private_key_path, private_key.as_bytes())
            .await
            .map_err(|err| io_error("写入私钥", &private_key_path, err))?;
        restrict_private_key(&private_key_path).await?;

        let public_key = if input.public_key.trim().is_empty() {
            derive_public_key(&private_key_path).await?
        } else {
            input.public_key.trim().to_owned()
        };
        validate_public_key(&public_key)?;
        let fingerprint = public_key_fingerprint(&public_key);

        let id = insert_credential(
            &self.db,
            InsertCredential {
                credential_key: &credential_key,
                name: &name,
                public_key: &public_key,
                private_key_path: &private_key_path.to_string_lossy(),
                fingerprint: &fingerprint,
                passphrase_hint: &passphrase_hint,
                created_by: &created_by,
            },
        )
        .await?;

        Ok(NodeCredentialCreated {
            id,
            name,
            public_key,
            fingerprint,
        })
    }

    pub async fn set_status(
        &self,
        credential_id: i64,
        status: &str,
    ) -> Result<(), NodeCredentialError> {
        let status = match status {
            "active" | "disabled" => status,
            _ => {
                return Err(NodeCredentialError::InvalidInput(
                    "凭据状态不支持".to_owned(),
                ));
            }
        };
        let result = sqlx::query(
            r#"
            UPDATE node_credentials
            SET status = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(credential_id)
        .bind(status)
        .execute(&self.db)
        .await?;
        if result.rows_affected() == 0 {
            return Err(NodeCredentialError::InvalidInput("凭据不存在".to_owned()));
        }
        Ok(())
    }

    fn credential_dir(&self, credential_key: &str) -> Result<PathBuf, NodeCredentialError> {
        validate_generated_key(credential_key)?;
        Ok(self.data_dir.join("credentials").join(credential_key))
    }
}

enum GeneratedKeyAlgorithm {
    Ed25519,
    Rsa4096,
}

impl GeneratedKeyAlgorithm {
    fn parse(value: &str) -> Result<Self, NodeCredentialError> {
        match value {
            "" | "ed25519" => Ok(Self::Ed25519),
            "rsa_4096" => Ok(Self::Rsa4096),
            _ => Err(NodeCredentialError::InvalidInput(
                "密钥类型不支持，请选择 ed25519 或 RSA 4096".to_owned(),
            )),
        }
    }

    fn ssh_keygen_type(&self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
            Self::Rsa4096 => "rsa",
        }
    }

    fn bits(&self) -> Option<u16> {
        match self {
            Self::Ed25519 => None,
            Self::Rsa4096 => Some(4096),
        }
    }

    fn private_key_file(&self) -> &'static str {
        match self {
            Self::Ed25519 => "id_ed25519",
            Self::Rsa4096 => "id_rsa",
        }
    }
}

struct InsertCredential<'a> {
    credential_key: &'a str,
    name: &'a str,
    public_key: &'a str,
    private_key_path: &'a str,
    fingerprint: &'a str,
    passphrase_hint: &'a str,
    created_by: &'a str,
}

async fn insert_credential(
    db: &SqlitePool,
    input: InsertCredential<'_>,
) -> Result<i64, NodeCredentialError> {
    let result = sqlx::query(
        r#"
        INSERT INTO node_credentials(
            credential_key,
            name,
            credential_type,
            public_key,
            private_key_path,
            fingerprint,
            passphrase_hint,
            status,
            created_by
        )
        VALUES (?1, ?2, 'ssh_key', ?3, ?4, ?5, ?6, 'active', ?7)
        "#,
    )
    .bind(input.credential_key)
    .bind(input.name)
    .bind(input.public_key)
    .bind(input.private_key_path)
    .bind(input.fingerprint)
    .bind(input.passphrase_hint)
    .bind(input.created_by)
    .execute(db)
    .await?;
    Ok(result.last_insert_rowid())
}

async fn derive_public_key(private_key_path: &Path) -> Result<String, NodeCredentialError> {
    let output = Command::new("ssh-keygen")
        .arg("-y")
        .arg("-f")
        .arg(private_key_path)
        .output()
        .await
        .map_err(|err| NodeCredentialError::Internal(format!("执行 ssh-keygen -y 失败: {err}")))?;
    if !output.status.success() {
        return Err(NodeCredentialError::InvalidInput(format!(
            "无法从私钥导出公钥，请确认私钥未加密或手动填写公钥: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn restrict_private_key(path: &Path) -> Result<(), NodeCredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|err| io_error("设置私钥权限", path, err))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn required_text(value: &str, message: &str) -> Result<String, NodeCredentialError> {
    let value = value.trim();
    if value.is_empty() {
        Err(NodeCredentialError::InvalidInput(message.to_owned()))
    } else if value.contains('\n') || value.contains('\r') {
        Err(NodeCredentialError::InvalidInput(format!(
            "{message}，且不能包含换行"
        )))
    } else {
        Ok(value.to_owned())
    }
}

fn normalize_private_key(value: &str) -> Result<String, NodeCredentialError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(NodeCredentialError::InvalidInput(
            "请输入私钥内容".to_owned(),
        ));
    }
    if value.contains('\0') {
        return Err(NodeCredentialError::InvalidInput(
            "私钥内容不能包含空字符".to_owned(),
        ));
    }
    if !value.contains("BEGIN OPENSSH PRIVATE KEY")
        && !value.contains("BEGIN RSA PRIVATE KEY")
        && !value.contains("BEGIN EC PRIVATE KEY")
    {
        return Err(NodeCredentialError::InvalidInput(
            "私钥格式不支持，请使用 OpenSSH / RSA / EC PEM 私钥".to_owned(),
        ));
    }
    Ok(format!("{value}\n"))
}

fn validate_public_key(value: &str) -> Result<(), NodeCredentialError> {
    let mut parts = value.split_whitespace();
    let key_type = parts.next().unwrap_or_default();
    let body = parts.next().unwrap_or_default();
    if body.is_empty()
        || !matches!(
            key_type,
            "ssh-ed25519"
                | "ssh-rsa"
                | "ecdsa-sha2-nistp256"
                | "ecdsa-sha2-nistp384"
                | "ecdsa-sha2-nistp521"
        )
    {
        return Err(NodeCredentialError::InvalidInput(
            "公钥格式不支持，请使用 ssh-ed25519、ssh-rsa 或 ecdsa 公钥".to_owned(),
        ));
    }
    general_purpose::STANDARD
        .decode(body)
        .map_err(|_| NodeCredentialError::InvalidInput("公钥主体不是有效 Base64".to_owned()))?;
    Ok(())
}

fn public_key_fingerprint(public_key: &str) -> String {
    let blob = public_key
        .split_whitespace()
        .nth(1)
        .and_then(|body| general_purpose::STANDARD.decode(body).ok())
        .unwrap_or_else(|| public_key.as_bytes().to_vec());
    let digest = Sha256::digest(blob);
    format!("SHA256:{}", general_purpose::STANDARD_NO_PAD.encode(digest))
}

fn generated_credential_key() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let mut random = [0_u8; 6];
    OsRng.fill_bytes(&mut random);
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("cred-{ts}-{suffix}")
}

fn validate_generated_key(value: &str) -> Result<(), NodeCredentialError> {
    if value.trim().is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(NodeCredentialError::InvalidInput(
            "凭据标识仅支持字母、数字、短横线和下划线".to_owned(),
        ));
    }
    Ok(())
}

fn io_error(action: &str, path: &Path, err: std::io::Error) -> NodeCredentialError {
    NodeCredentialError::Internal(format!("{action} {} 失败: {err}", path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqliteConnectOptions;
    use tempfile::{TempDir, tempdir};

    use super::*;

    const PUBLIC_KEY: &str = "ssh-ed25519 YWJjZGVm test@example";

    async fn credential_service() -> (NodeCredentialService, SqlitePool, TempDir) {
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
        let data_dir = tempdir().expect("create data dir");
        (
            NodeCredentialService::new(db.clone(), data_dir.path()),
            db,
            data_dir,
        )
    }

    #[tokio::test]
    async fn uploaded_key_is_persisted_listed_and_disabled() {
        let (service, db, _data_dir) = credential_service().await;

        let created = service
            .create_uploaded_key(CreateUploadedCredentialInput {
                name: "  prod ssh key  ".to_owned(),
                private_key:
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----"
                        .to_owned(),
                public_key: PUBLIC_KEY.to_owned(),
                passphrase_hint: "vault item".to_owned(),
                created_by: "admin".to_owned(),
            })
            .await
            .expect("create uploaded key");

        assert_eq!(created.name, "prod ssh key");
        assert_eq!(created.public_key, PUBLIC_KEY);
        assert!(created.fingerprint.starts_with("SHA256:"));

        sqlx::query("UPDATE nodes SET credential_id = ?1 WHERE node_key = 'local'")
            .bind(created.id)
            .execute(&db)
            .await
            .expect("bind credential to local node");

        let credentials = service.list_credentials().await.expect("list credentials");
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].name, "prod ssh key");
        assert_eq!(credentials[0].status, "active");
        assert_eq!(credentials[0].passphrase_hint, "vault item");
        assert_eq!(credentials[0].bound_node_count, 1);
        assert!(Path::new(&credentials[0].private_key_path).is_file());

        let options = service.active_options().await.expect("active options");
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, created.id);

        service
            .set_status(created.id, "disabled")
            .await
            .expect("disable credential");

        assert!(
            service
                .active_options()
                .await
                .expect("active options after disable")
                .is_empty()
        );
        let credentials = service
            .list_credentials()
            .await
            .expect("list after disable");
        assert_eq!(credentials[0].status, "disabled");
    }

    #[tokio::test]
    async fn uploaded_key_rejects_bad_inputs() {
        let (service, _db, _data_dir) = credential_service().await;

        let empty_name = service
            .create_uploaded_key(CreateUploadedCredentialInput {
                name: " ".to_owned(),
                private_key:
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----"
                        .to_owned(),
                public_key: PUBLIC_KEY.to_owned(),
                passphrase_hint: String::new(),
                created_by: "admin".to_owned(),
            })
            .await
            .expect_err("empty name should be rejected");
        assert!(matches!(empty_name, NodeCredentialError::InvalidInput(_)));

        let bad_public_key = service
            .create_uploaded_key(CreateUploadedCredentialInput {
                name: "bad public key".to_owned(),
                private_key:
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----"
                        .to_owned(),
                public_key: "ssh-ed25519 not-base64".to_owned(),
                passphrase_hint: String::new(),
                created_by: "admin".to_owned(),
            })
            .await
            .expect_err("bad public key should be rejected");
        assert!(matches!(
            bad_public_key,
            NodeCredentialError::InvalidInput(_)
        ));

        let bad_status = service
            .set_status(404, "deleted")
            .await
            .expect_err("bad status should be rejected");
        assert!(matches!(bad_status, NodeCredentialError::InvalidInput(_)));

        let missing_credential = service
            .set_status(404, "active")
            .await
            .expect_err("missing credential should be rejected");
        assert!(matches!(
            missing_credential,
            NodeCredentialError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    async fn credential_error_and_path_helpers_cover_edges() {
        let (service, _db, _data_dir) = credential_service().await;

        let conflict = NodeCredentialError::Conflict("exists".to_owned());
        assert_eq!(conflict.message(), "exists");
        assert_eq!(conflict.to_string(), "exists");
        let internal = NodeCredentialError::from(sqlx::Error::RowNotFound);
        assert!(matches!(internal, NodeCredentialError::Internal(_)));
        assert!(!internal.message().trim().is_empty());

        let credential_dir = service
            .credential_dir("cred-abc_123")
            .expect("valid credential dir");
        assert!(credential_dir.ends_with(Path::new("credentials").join("cred-abc_123")));
        assert!(service.credential_dir("bad key").is_err());
        let generated = generated_credential_key();
        assert!(generated.starts_with("cred-"));
        validate_generated_key(&generated).expect("generated key is valid");

        assert!(public_key_fingerprint("not-a-public-key").starts_with("SHA256:"));
        validate_public_key("ecdsa-sha2-nistp256 YWJj comment").expect("ecdsa key");
        assert!(validate_public_key("ssh-ed25519").is_err());
        assert!(
            normalize_private_key(
                "-----BEGIN EC PRIVATE KEY-----\nfake\n-----END EC PRIVATE KEY-----"
            )
            .is_ok()
        );
    }

    #[test]
    fn generated_key_algorithm_accepts_supported_values_only() {
        let ed25519 = GeneratedKeyAlgorithm::parse("").expect("default algorithm");
        let rsa = GeneratedKeyAlgorithm::parse("rsa_4096").expect("rsa algorithm");

        assert!(matches!(ed25519, GeneratedKeyAlgorithm::Ed25519));
        assert_eq!(ed25519.ssh_keygen_type(), "ed25519");
        assert_eq!(ed25519.bits(), None);
        assert_eq!(ed25519.private_key_file(), "id_ed25519");
        assert!(matches!(rsa, GeneratedKeyAlgorithm::Rsa4096));
        assert_eq!(rsa.ssh_keygen_type(), "rsa");
        assert_eq!(rsa.bits(), Some(4096));
        assert_eq!(rsa.private_key_file(), "id_rsa");
        assert!(GeneratedKeyAlgorithm::parse("dsa").is_err());
    }

    #[test]
    fn normalizes_and_rejects_private_key_content() {
        let normalized = normalize_private_key(
            "  -----BEGIN RSA PRIVATE KEY-----\nfake\n-----END RSA PRIVATE KEY-----  ",
        )
        .expect("valid rsa key");

        assert!(normalized.ends_with('\n'));
        assert!(normalize_private_key("").is_err());
        assert!(normalize_private_key("plain text").is_err());
        assert!(normalize_private_key("-----BEGIN OPENSSH PRIVATE KEY-----\0").is_err());
    }

    #[test]
    fn validates_required_text_and_generated_key() {
        assert_eq!(
            required_text("  prod key  ", "name required").expect("required text"),
            "prod key"
        );
        assert!(required_text("line\nbreak", "name required").is_err());
        assert!(required_text(" ", "name required").is_err());
        assert!(validate_generated_key("cred-abc_123").is_ok());
        assert!(validate_generated_key("bad key").is_err());
    }

    #[test]
    fn validates_public_key_and_builds_stable_fingerprint() {
        validate_public_key(PUBLIC_KEY).expect("valid public key");
        assert!(validate_public_key("ssh-dss AAAA").is_err());
        assert!(validate_public_key("ssh-ed25519 not-base64").is_err());

        assert_eq!(
            public_key_fingerprint(PUBLIC_KEY),
            public_key_fingerprint(PUBLIC_KEY)
        );
        assert!(public_key_fingerprint(PUBLIC_KEY).starts_with("SHA256:"));
    }
}
