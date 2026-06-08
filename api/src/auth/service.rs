use std::{collections::BTreeMap, fmt, time::Duration};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool, sqlite::SqliteRow};

use super::{
    all_permissions,
    password::{hash_password, validate_password, verify_password},
    permission_dependencies,
    session_store::{DynSessionStore, SessionSnapshot},
    token::{generate_token, hash_token},
};

const ACCESS_TTL_SECS: i64 = 2 * 60 * 60;
const REFRESH_TTL_SECS: i64 = 14 * 24 * 60 * 60;
const MAX_FAILED_LOGIN_ATTEMPTS: i64 = 5;
const BOOTSTRAP_SUPER_ADMIN_USERNAME: &str = "admin";

#[derive(Clone, Copy)]
enum BuiltinPermissionScope {
    AllCurrent,
    Explicit(&'static [&'static str]),
}

#[derive(Clone, Copy)]
struct BuiltinRolePolicy {
    role_code: &'static str,
    permissions: BuiltinPermissionScope,
}

const DEPLOYER_PERMISSION_KEYS: &[&str] = &[
    "dashboard.view",
    "apps.view",
    "services.view",
    "services.deploy",
    "services.logs",
    "services.rollback",
    "nodes.view",
    "node_credentials.view",
    "tasks.view",
    "tasks.cancel",
    "tasks.retry",
    "templates.view",
    "artifacts.view",
    "profile.view",
    "profile.password.change",
];

const OPERATOR_PERMISSION_KEYS: &[&str] = &[
    "dashboard.view",
    "apps.view",
    "apps.create",
    "apps.update",
    "apps.status",
    "services.view",
    "services.logs",
    "nodes.view",
    "nodes.manage",
    "nodes.install",
    "node_credentials.view",
    "node_credentials.manage",
    "tasks.view",
    "templates.view",
    "artifacts.view",
    "artifacts.upload",
    "settings.view",
    "profile.view",
    "profile.password.change",
];

const VIEWER_PERMISSION_KEYS: &[&str] = &[
    "dashboard.view",
    "apps.view",
    "services.view",
    "nodes.view",
    "node_credentials.view",
    "tasks.view",
    "templates.view",
    "artifacts.view",
    "settings.view",
    "profile.view",
    "profile.password.change",
];

const AUDITOR_PERMISSION_KEYS: &[&str] = &[
    "dashboard.view",
    "apps.view",
    "services.view",
    "nodes.view",
    "node_credentials.view",
    "tasks.view",
    "audit.view",
    "profile.view",
    "profile.password.change",
];

const BUILTIN_ROLE_POLICIES: &[BuiltinRolePolicy] = &[
    BuiltinRolePolicy {
        role_code: "super_admin",
        permissions: BuiltinPermissionScope::AllCurrent,
    },
    BuiltinRolePolicy {
        role_code: "admin",
        permissions: BuiltinPermissionScope::AllCurrent,
    },
    BuiltinRolePolicy {
        role_code: "deployer",
        permissions: BuiltinPermissionScope::Explicit(DEPLOYER_PERMISSION_KEYS),
    },
    BuiltinRolePolicy {
        role_code: "operator",
        permissions: BuiltinPermissionScope::Explicit(OPERATOR_PERMISSION_KEYS),
    },
    BuiltinRolePolicy {
        role_code: "viewer",
        permissions: BuiltinPermissionScope::Explicit(VIEWER_PERMISSION_KEYS),
    },
    BuiltinRolePolicy {
        role_code: "auditor",
        permissions: BuiltinPermissionScope::Explicit(AUDITOR_PERMISSION_KEYS),
    },
];

#[derive(Debug)]
pub enum AuthError {
    InvalidInput(String),
    Unauthorized(String),
    Forbidden(String),
    Conflict(String),
    Internal(String),
}

impl AuthError {
    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(message)
            | Self::Unauthorized(message)
            | Self::Forbidden(message)
            | Self::Conflict(message)
            | Self::Internal(message) => message,
        }
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for AuthError {}

impl From<sqlx::Error> for AuthError {
    fn from(value: sqlx::Error) -> Self {
        Self::Internal(format!("数据库操作失败: {value}"))
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (self.status_code(), self.message().to_owned()).into_response()
    }
}

#[derive(Clone)]
pub struct AuthService {
    db: SqlitePool,
    store: DynSessionStore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthAccount {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub status: String,
    pub is_super_admin: bool,
}

#[derive(Clone, Debug)]
pub struct CurrentSession {
    pub session_id: i64,
    pub account: AuthAccount,
    pub role_codes: Vec<String>,
    pub permission_keys: Vec<String>,
    pub is_super_admin: bool,
    pub csrf_token: String,
    pub access_token_hash: String,
}

impl CurrentSession {
    pub fn display_name(&self) -> &str {
        if self.account.display_name.trim().is_empty() {
            &self.account.username
        } else {
            &self.account.display_name
        }
    }

    pub fn can(&self, permission_key: &str) -> bool {
        self.is_super_admin
            || self
                .permission_keys
                .iter()
                .any(|permission| permission == permission_key)
    }
}

#[derive(Debug)]
pub struct LoginInput {
    pub username: String,
    pub password: String,
    pub display_name: Option<String>,
    pub client_ip: String,
    pub user_agent: String,
}

#[derive(Clone, Debug)]
pub struct SessionTokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Clone, Debug)]
pub struct AuthResult {
    pub session: CurrentSession,
    pub tokens: SessionTokens,
}

struct AuditRecord<'a> {
    actor_account_id: Option<i64>,
    actor_username: &'a str,
    action: &'a str,
    target_type: &'a str,
    target_id: &'a str,
    message: &'a str,
    ip: &'a str,
    user_agent: &'a str,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AccountListItem {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub status: String,
    pub is_super_admin: i64,
    pub failed_login_attempts: i64,
    pub locked_at: Option<String>,
    pub locked_reason: String,
    pub last_login_at: Option<String>,
    pub role_names: Option<String>,
    pub role_ids: Option<String>,
    pub active_session_count: i64,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RoleListItem {
    pub id: i64,
    pub role_code: String,
    pub role_name: String,
    pub description: String,
    pub status: String,
    pub is_system: i64,
    pub permission_count: i64,
    pub permission_ids: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RoleOption {
    pub id: i64,
    pub role_code: String,
    pub role_name: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct SessionListItem {
    pub id: i64,
    pub account_id: i64,
    pub username: String,
    pub display_name: String,
    pub session_status: String,
    pub access_expires_at: String,
    pub refresh_expires_at: String,
    pub last_ip: String,
    pub user_agent: String,
    pub created_at: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ApiTokenListItem {
    pub id: i64,
    pub account_id: i64,
    pub username: String,
    pub display_name: String,
    pub token_prefix: String,
    pub source: String,
    pub status: String,
    pub last_used_at: Option<String>,
    pub last_used_ip: String,
    pub revoked_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct CreatedApiToken {
    pub id: i64,
    pub token: String,
    pub token_prefix: String,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct ApiTokenAuthSession {
    pub session: CurrentSession,
    pub token_id: i64,
    pub source: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AuditLogItem {
    pub id: i64,
    pub actor_username: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub message: String,
    pub ip: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Default)]
pub struct AuditLogFilter {
    pub action: Option<String>,
    pub target_type: Option<String>,
    pub actor: Option<String>,
    pub query: Option<String>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AuditFilterOption {
    pub value: String,
}

impl AuthService {
    pub fn new(db: SqlitePool, store: DynSessionStore) -> Self {
        Self { db, store }
    }

    pub fn db(&self) -> &SqlitePool {
        &self.db
    }

    pub async fn sync_permission_registry(&self) -> Result<(), AuthError> {
        let mut tx = self.db.begin().await?;
        for permission in all_permissions() {
            sqlx::query(
                r#"
                INSERT INTO admin_permissions(
                    permission_key,
                    permission_name,
                    description,
                    resource_type,
                    resource_key,
                    module
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(permission_key) DO UPDATE SET
                    permission_name = excluded.permission_name,
                    description = excluded.description,
                    resource_type = excluded.resource_type,
                    resource_key = excluded.resource_key,
                    module = excluded.module,
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                "#,
            )
            .bind(permission.key)
            .bind(permission.name)
            .bind(permission.description)
            .bind(permission.resource_type.as_str())
            .bind(permission.resource_key)
            .bind(permission.module)
            .execute(&mut *tx)
            .await?;
        }
        delete_stale_permissions(&mut tx).await?;
        sync_builtin_role_permissions(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn bootstrap_required(&self) -> Result<bool, AuthError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_accounts")
            .fetch_one(&self.db)
            .await?;
        Ok(count == 0)
    }

    pub async fn bootstrap_init(&self, input: LoginInput) -> Result<AuthResult, AuthError> {
        let username = BOOTSTRAP_SUPER_ADMIN_USERNAME.to_owned();
        validate_password(&input.password)?;
        if !self.bootstrap_required().await? {
            return Err(AuthError::Conflict("平台管理员已经初始化".to_owned()));
        }

        let password_hash = hash_password(&input.password)?;
        let display_name = input
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&username)
            .to_owned();

        let mut tx = self.db.begin().await?;
        let account_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO admin_accounts(
                username,
                password_hash,
                display_name,
                status,
                is_super_admin,
                password_changed_at
            )
            VALUES (?1, ?2, ?3, 'active', 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            RETURNING id
            "#,
        )
        .bind(&username)
        .bind(password_hash)
        .bind(display_name)
        .fetch_one(&mut *tx)
        .await?;

        let super_admin_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'super_admin'")
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO admin_account_roles(account_id, role_id) VALUES (?1, ?2)",
        )
        .bind(account_id)
        .bind(super_admin_role_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        self.record_audit(AuditRecord {
            actor_account_id: Some(account_id),
            actor_username: &username,
            action: "auth.bootstrap",
            target_type: "account",
            target_id: &account_id.to_string(),
            message: "初始化超级管理员",
            ip: &input.client_ip,
            user_agent: &input.user_agent,
        })
        .await?;
        self.login(LoginInput {
            username,
            password: input.password,
            display_name: None,
            client_ip: input.client_ip,
            user_agent: input.user_agent,
        })
        .await
    }

    pub async fn login(&self, input: LoginInput) -> Result<AuthResult, AuthError> {
        let username = normalize_username(&input.username)?;
        if input.password.trim().is_empty() {
            return Err(AuthError::InvalidInput("请输入密码".to_owned()));
        }

        let row = sqlx::query(
            r#"
            SELECT
                id,
                username,
                password_hash,
                display_name,
                status,
                is_super_admin,
                failed_login_attempts
            FROM admin_accounts
            WHERE username = ?1
            "#,
        )
        .bind(&username)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else {
            self.record_audit(AuditRecord {
                actor_account_id: None,
                actor_username: &username,
                action: "auth.login_failed",
                target_type: "account",
                target_id: "",
                message: "账号不存在",
                ip: &input.client_ip,
                user_agent: &input.user_agent,
            })
            .await?;
            return Err(AuthError::Unauthorized("用户名或密码错误".to_owned()));
        };

        let account = row_to_account(&row);
        if account.status != "active" {
            let message = if account.status == "locked" {
                "账号已锁定"
            } else {
                "账号未启用"
            };
            self.record_audit(AuditRecord {
                actor_account_id: Some(account.id),
                actor_username: &account.username,
                action: "auth.login_failed",
                target_type: "account",
                target_id: &account.id.to_string(),
                message,
                ip: &input.client_ip,
                user_agent: &input.user_agent,
            })
            .await?;
            return Err(AuthError::Forbidden(message.to_owned()));
        }
        let password_hash: String = row.get("password_hash");
        if !verify_password(&input.password, &password_hash)? {
            let failed_attempts = row.get::<i64, _>("failed_login_attempts") + 1;
            let locked = failed_attempts >= MAX_FAILED_LOGIN_ATTEMPTS;
            let failed_message = if locked {
                format!("密码错误，账号已锁定（连续失败 {failed_attempts} 次）")
            } else {
                format!("密码错误（连续失败 {failed_attempts} 次）")
            };
            self.record_failed_login(account.id, failed_attempts, locked)
                .await?;
            self.record_audit(AuditRecord {
                actor_account_id: Some(account.id),
                actor_username: &account.username,
                action: "auth.login_failed",
                target_type: "account",
                target_id: &account.id.to_string(),
                message: &failed_message,
                ip: &input.client_ip,
                user_agent: &input.user_agent,
            })
            .await?;
            if locked {
                self.revoke_sessions_by_account(account.id, "account_locked")
                    .await?;
            }
            return Err(AuthError::Unauthorized("用户名或密码错误".to_owned()));
        }

        let result = self
            .issue_session(account, &input.client_ip, &input.user_agent)
            .await?;
        sqlx::query(
            r#"
            UPDATE admin_accounts
            SET last_login_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                last_login_ip = ?2,
                failed_login_attempts = 0,
                locked_at = NULL,
                locked_reason = '',
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(result.session.account.id)
        .bind(&input.client_ip)
        .execute(&self.db)
        .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(result.session.account.id),
            actor_username: &result.session.account.username,
            action: "auth.login",
            target_type: "account",
            target_id: &result.session.account.id.to_string(),
            message: "登录成功",
            ip: &input.client_ip,
            user_agent: &input.user_agent,
        })
        .await?;
        Ok(result)
    }

    pub async fn authenticate_access_token(
        &self,
        access_token: &str,
    ) -> Result<CurrentSession, AuthError> {
        let access_hash = hash_token(access_token);
        let Some(snapshot) = self.store.get_by_access_hash(&access_hash).await? else {
            return Err(AuthError::Unauthorized("登录已过期，请重新登录".to_owned()));
        };
        let active = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM admin_sessions s
            JOIN admin_accounts a ON a.id = s.account_id
            WHERE s.id = ?1
              AND s.session_status = 'active'
              AND a.status = 'active'
              AND s.access_token_hash = ?2
              AND s.access_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(snapshot.session_id)
        .bind(&access_hash)
        .fetch_one(&self.db)
        .await?;
        if active == 0 {
            let _ = self.store.delete_session(&snapshot).await;
            return Err(AuthError::Unauthorized("登录已过期，请重新登录".to_owned()));
        }
        Ok(snapshot.current_session())
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<AuthResult, AuthError> {
        let refresh_hash = hash_token(refresh_token);
        let Some(snapshot) = self.store.get_by_refresh_hash(&refresh_hash).await? else {
            return Err(AuthError::Unauthorized(
                "登录续期失败，请重新登录".to_owned(),
            ));
        };
        let row = sqlx::query(
            r#"
            SELECT
                s.id AS session_id,
                a.id,
                a.username,
                a.password_hash,
                a.display_name,
                a.status,
                a.is_super_admin
            FROM admin_sessions s
            JOIN admin_accounts a ON a.id = s.account_id
            WHERE s.refresh_token_hash = ?1
              AND s.id = ?2
              AND s.session_status = 'active'
              AND s.refresh_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(&refresh_hash)
        .bind(snapshot.session_id)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else {
            let _ = self.store.delete_session(&snapshot).await;
            return Err(AuthError::Unauthorized(
                "登录续期失败，请重新登录".to_owned(),
            ));
        };
        let account = row_to_account(&row);
        if account.status != "active" {
            let _ = self.store.delete_session(&snapshot).await;
            return Err(AuthError::Forbidden("账号未启用".to_owned()));
        }
        let session_id: i64 = row.get("session_id");
        let result = self.issue_session(account, "", "").await?;
        self.revoke_session_record(session_id, "refresh_rotated")
            .await?;
        self.store.delete_session_by_id(session_id).await?;
        Ok(result)
    }

    pub async fn logout(&self, session: &CurrentSession) -> Result<(), AuthError> {
        sqlx::query(
            r#"
            UPDATE admin_sessions
            SET session_status = 'revoked',
                revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                revoke_reason = 'logout',
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(session.session_id)
        .execute(&self.db)
        .await?;
        self.store.delete_session_by_id(session.session_id).await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(session.account.id),
            actor_username: &session.account.username,
            action: "auth.logout",
            target_type: "account",
            target_id: &session.account.id.to_string(),
            message: "退出登录",
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn list_accounts(&self) -> Result<Vec<AccountListItem>, AuthError> {
        sqlx::query_as::<_, AccountListItem>(
            r#"
            SELECT
                a.id,
                a.username,
                a.display_name,
                a.status,
                a.is_super_admin,
                a.failed_login_attempts,
                a.locked_at,
                a.locked_reason,
                a.last_login_at,
                group_concat(r.role_name, '、') AS role_names,
                group_concat(r.id, ',') AS role_ids,
                (
                    SELECT COUNT(*)
                    FROM admin_sessions s
                    WHERE s.account_id = a.id
                      AND s.session_status = 'active'
                      AND s.refresh_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                ) AS active_session_count
            FROM admin_accounts a
            LEFT JOIN admin_account_roles ar ON ar.account_id = a.id
            LEFT JOIN admin_roles r ON r.id = ar.role_id
            GROUP BY a.id
            ORDER BY a.id ASC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn list_roles(&self) -> Result<Vec<RoleListItem>, AuthError> {
        sqlx::query_as::<_, RoleListItem>(
            r#"
            SELECT
                r.id,
                r.role_code,
                r.role_name,
                r.description,
                r.status,
                r.is_system,
                COUNT(rp.permission_id) AS permission_count,
                group_concat(rp.permission_id, ',') AS permission_ids
            FROM admin_roles r
            LEFT JOIN admin_role_permissions rp ON rp.role_id = r.id
            GROUP BY r.id
            ORDER BY r.id ASC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn list_role_options(&self) -> Result<Vec<RoleOption>, AuthError> {
        sqlx::query_as::<_, RoleOption>(
            r#"
            SELECT id, role_code, role_name
            FROM admin_roles
            WHERE status = 'active'
            ORDER BY id ASC
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn permission_groups(
        &self,
    ) -> Result<BTreeMap<String, Vec<super::PermissionView>>, AuthError> {
        let rows = sqlx::query_as::<_, super::PermissionView>(
            r#"
            SELECT id, permission_key, permission_name, description, resource_type, module
            FROM admin_permissions
            ORDER BY module, resource_type DESC, permission_key
            "#,
        )
        .fetch_all(&self.db)
        .await?;
        let mut groups = BTreeMap::<String, Vec<super::PermissionView>>::new();
        for row in rows {
            groups.entry(row.module.clone()).or_default().push(row);
        }
        Ok(groups)
    }

    pub async fn create_account(
        &self,
        actor: &CurrentSession,
        username: &str,
        display_name: &str,
        password: &str,
        role_ids: &[i64],
    ) -> Result<(), AuthError> {
        let username = normalize_username(username)?;
        let password_hash = hash_password(password)?;
        let display_name = display_name.trim();
        let display_name = if display_name.is_empty() {
            username.as_str()
        } else {
            display_name
        };
        let role_ids = self.valid_active_role_ids(role_ids).await?;
        let role_summary = self.role_names_summary(&role_ids).await?;
        let mut tx = self.db.begin().await?;
        let account_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO admin_accounts(username, password_hash, display_name, status, password_changed_at)
            VALUES (?1, ?2, ?3, 'active', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            RETURNING id
            "#,
        )
        .bind(&username)
        .bind(password_hash)
        .bind(display_name)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| match err {
            sqlx::Error::Database(db_err) if db_err.is_unique_violation() => {
                AuthError::Conflict("用户名已存在".to_owned())
            }
            other => AuthError::from(other),
        })?;
        replace_account_roles(&mut tx, account_id, &role_ids).await?;
        tx.commit().await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.account_create",
            target_type: "account",
            target_id: &account_id.to_string(),
            message: &format!(
                "创建账号 {} ({username})，初始角色：{role_summary}",
                display_name
            ),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn set_account_status(
        &self,
        actor: &CurrentSession,
        account_id: i64,
        status: &str,
    ) -> Result<(), AuthError> {
        let status = match status {
            "active" | "disabled" | "locked" => status,
            _ => return Err(AuthError::InvalidInput("账号状态无效".to_owned())),
        };
        if account_id == actor.account.id && status != "active" {
            return Err(AuthError::InvalidInput(
                "不能禁用或锁定当前登录账号".to_owned(),
            ));
        }
        let account_label = self.account_label(account_id).await?;
        sqlx::query(
            r#"
            UPDATE admin_accounts
            SET status = ?2,
                failed_login_attempts = CASE WHEN ?2 = 'active' THEN 0 ELSE failed_login_attempts END,
                locked_at = CASE
                    WHEN ?2 = 'locked' AND locked_at IS NULL THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                    WHEN ?2 = 'active' THEN NULL
                    ELSE locked_at
                END,
                locked_reason = CASE
                    WHEN ?2 = 'locked' THEN '管理员手动锁定'
                    WHEN ?2 = 'active' THEN ''
                    ELSE locked_reason
                END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(account_id)
        .bind(status)
        .execute(&self.db)
        .await?;
        let revoked_count = if status != "active" {
            let reason = if status == "locked" {
                "account_locked"
            } else {
                "account_disabled"
            };
            self.revoke_sessions_by_account(account_id, reason).await?
        } else {
            0
        };
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.account_status",
            target_type: "account",
            target_id: &account_id.to_string(),
            message: &format!(
                "更新账号 {account_label} 状态为{}，撤销活跃会话 {revoked_count} 个",
                account_status_name(status)
            ),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn reset_account_password(
        &self,
        actor: &CurrentSession,
        account_id: i64,
        password: &str,
    ) -> Result<(), AuthError> {
        if account_id == actor.account.id {
            return Err(AuthError::InvalidInput(
                "不能在账号管理中重置当前登录账号密码，请使用个人中心".to_owned(),
            ));
        }
        let account_label = self.account_label(account_id).await?;
        let password_hash = hash_password(password)?;
        sqlx::query(
            r#"
            UPDATE admin_accounts
            SET password_hash = ?2,
                password_changed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(account_id)
        .bind(password_hash)
        .execute(&self.db)
        .await?;
        let revoked_count = self
            .revoke_sessions_by_account(account_id, "password_reset")
            .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.account_reset_password",
            target_type: "account",
            target_id: &account_id.to_string(),
            message: &format!("重置账号 {account_label} 密码，撤销活跃会话 {revoked_count} 个"),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn update_account_roles(
        &self,
        actor: &CurrentSession,
        account_id: i64,
        role_ids: &[i64],
    ) -> Result<(), AuthError> {
        if account_id == actor.account.id {
            return Err(AuthError::InvalidInput(
                "不能修改当前登录账号的角色".to_owned(),
            ));
        }
        let account_label = self.account_label(account_id).await?;
        let role_ids = self.valid_active_role_ids(role_ids).await?;
        let previous_roles = self.account_role_names_summary(account_id).await?;
        let next_roles = self.role_names_summary(&role_ids).await?;
        let mut tx = self.db.begin().await?;
        replace_account_roles(&mut tx, account_id, &role_ids).await?;
        tx.commit().await?;
        let revoked_count = self
            .revoke_sessions_by_account(account_id, "role_changed")
            .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.account_roles",
            target_type: "account",
            target_id: &account_id.to_string(),
            message: &format!(
                "更新账号 {account_label} 角色：{previous_roles} -> {next_roles}，撤销活跃会话 {revoked_count} 个"
            ),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn create_role(
        &self,
        actor: &CurrentSession,
        role_code: &str,
        role_name: &str,
        description: &str,
        permission_ids: &[i64],
    ) -> Result<(), AuthError> {
        let role_code = normalize_role_code(role_code)?;
        let role_name = role_name.trim();
        if role_name.is_empty() {
            return Err(AuthError::InvalidInput("请输入角色名称".to_owned()));
        }
        let permission_ids = self.normalized_permission_ids(permission_ids).await?;
        let permission_count = permission_ids.len();
        let mut tx = self.db.begin().await?;
        let role_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO admin_roles(role_code, role_name, description, status, is_system)
            VALUES (?1, ?2, ?3, 'active', 0)
            RETURNING id
            "#,
        )
        .bind(&role_code)
        .bind(role_name)
        .bind(description.trim())
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| match err {
            sqlx::Error::Database(db_err) if db_err.is_unique_violation() => {
                AuthError::Conflict("角色编码已存在".to_owned())
            }
            other => AuthError::from(other),
        })?;
        replace_role_permissions(&mut tx, role_id, &permission_ids).await?;
        tx.commit().await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.role_create",
            target_type: "role",
            target_id: &role_id.to_string(),
            message: &format!("创建角色 {role_name} ({role_code})，初始权限 {permission_count} 个"),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn set_role_status(
        &self,
        actor: &CurrentSession,
        role_id: i64,
        status: &str,
    ) -> Result<(), AuthError> {
        let status = match status {
            "active" | "disabled" => status,
            _ => return Err(AuthError::InvalidInput("角色状态无效".to_owned())),
        };
        self.ensure_custom_role(role_id).await?;
        let role_label = self.role_label(role_id).await?;
        sqlx::query(
            r#"
            UPDATE admin_roles
            SET status = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(role_id)
        .bind(status)
        .execute(&self.db)
        .await?;
        self.revoke_sessions_by_role(role_id, "role_status_changed")
            .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.role_status",
            target_type: "role",
            target_id: &role_id.to_string(),
            message: &format!(
                "更新角色 {role_label} 状态为 {}",
                if status == "active" {
                    "启用"
                } else {
                    "禁用"
                }
            ),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn update_role_permissions(
        &self,
        actor: &CurrentSession,
        role_id: i64,
        permission_ids: &[i64],
    ) -> Result<(), AuthError> {
        self.ensure_custom_role(role_id).await?;
        let role_label = self.role_label(role_id).await?;
        let permission_ids = self.normalized_permission_ids(permission_ids).await?;
        let previous_permission_count = self.role_permission_count(role_id).await?;
        let next_permission_count = permission_ids.len();
        let mut tx = self.db.begin().await?;
        replace_role_permissions(&mut tx, role_id, &permission_ids).await?;
        tx.commit().await?;
        self.revoke_sessions_by_role(role_id, "role_permissions_changed")
            .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.role_permissions",
            target_type: "role",
            target_id: &role_id.to_string(),
            message: &format!(
                "更新角色 {role_label} 权限：{previous_permission_count} -> {next_permission_count}"
            ),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    async fn role_label(&self, role_id: i64) -> Result<String, AuthError> {
        let row = sqlx::query("SELECT role_name, role_code FROM admin_roles WHERE id = ?1")
            .bind(role_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| AuthError::InvalidInput("角色不存在".to_owned()))?;
        let role_name: String = row.get("role_name");
        let role_code: String = row.get("role_code");
        Ok(format!("{role_name} ({role_code})"))
    }

    async fn valid_active_role_ids(&self, role_ids: &[i64]) -> Result<Vec<i64>, AuthError> {
        let ids = unique_ids(role_ids);
        if ids.is_empty() {
            return Err(AuthError::InvalidInput(
                "账号至少需要分配一个启用角色".to_owned(),
            ));
        }
        for role_id in &ids {
            let exists = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM admin_roles WHERE id = ?1 AND status = 'active'",
            )
            .bind(role_id)
            .fetch_one(&self.db)
            .await?;
            if exists == 0 {
                return Err(AuthError::InvalidInput("角色不存在或已禁用".to_owned()));
            }
        }
        Ok(ids)
    }

    async fn normalized_permission_ids(
        &self,
        permission_ids: &[i64],
    ) -> Result<Vec<i64>, AuthError> {
        let mut ids = unique_ids(permission_ids);
        if ids.is_empty() {
            return Ok(ids);
        }

        let rows = sqlx::query(
            r#"
            SELECT id, permission_key
            FROM admin_permissions
            ORDER BY id ASC
            "#,
        )
        .fetch_all(&self.db)
        .await?;
        let id_to_key = rows
            .iter()
            .map(|row| {
                (
                    row.get::<i64, _>("id"),
                    row.get::<String, _>("permission_key"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let key_to_id = id_to_key
            .iter()
            .map(|(id, key)| (key.clone(), *id))
            .collect::<BTreeMap<_, _>>();

        for permission_id in &ids {
            if !id_to_key.contains_key(permission_id) {
                return Err(AuthError::InvalidInput("权限不存在".to_owned()));
            }
        }

        let mut cursor = 0;
        while cursor < ids.len() {
            let Some(permission_key) = id_to_key.get(&ids[cursor]) else {
                cursor += 1;
                continue;
            };
            for dependency_key in permission_dependencies(permission_key) {
                let Some(dependency_id) = key_to_id.get(*dependency_key) else {
                    continue;
                };
                if !ids.contains(dependency_id) {
                    ids.push(*dependency_id);
                }
            }
            cursor += 1;
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    async fn account_label(&self, account_id: i64) -> Result<String, AuthError> {
        let row = sqlx::query("SELECT username, display_name FROM admin_accounts WHERE id = ?1")
            .bind(account_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| AuthError::InvalidInput("账号不存在".to_owned()))?;
        let username: String = row.get("username");
        let display_name: String = row.get("display_name");
        Ok(format_account_label(&username, &display_name))
    }

    async fn account_role_names_summary(&self, account_id: i64) -> Result<String, AuthError> {
        let names = sqlx::query_scalar::<_, String>(
            r#"
            SELECT r.role_name
            FROM admin_roles r
            JOIN admin_account_roles ar ON ar.role_id = r.id
            WHERE ar.account_id = ?1
            ORDER BY r.id ASC
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.db)
        .await?;
        Ok(role_summary_from_names(names))
    }

    async fn role_names_summary(&self, role_ids: &[i64]) -> Result<String, AuthError> {
        let ids = unique_ids(role_ids);
        if ids.is_empty() {
            return Ok("未分配".to_owned());
        }
        let mut names = Vec::with_capacity(ids.len());
        for role_id in ids {
            let name = sqlx::query_scalar::<_, String>(
                "SELECT role_name FROM admin_roles WHERE id = ?1 AND status = 'active'",
            )
            .bind(role_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| AuthError::InvalidInput("角色不存在或已禁用".to_owned()))?;
            names.push(name);
        }
        Ok(role_summary_from_names(names))
    }

    async fn role_permission_count(&self, role_id: i64) -> Result<usize, AuthError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM admin_role_permissions WHERE role_id = ?1")
                .bind(role_id)
                .fetch_one(&self.db)
                .await?;
        Ok(count.max(0) as usize)
    }

    async fn ensure_custom_role(&self, role_id: i64) -> Result<(), AuthError> {
        let is_system =
            sqlx::query_scalar::<_, i64>("SELECT is_system FROM admin_roles WHERE id = ?1")
                .bind(role_id)
                .fetch_optional(&self.db)
                .await?
                .ok_or_else(|| AuthError::InvalidInput("角色不存在".to_owned()))?;
        if is_system == 1 {
            return Err(AuthError::InvalidInput(
                "系统内置角色由平台版本维护，不能在页面中修改".to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn change_own_password(
        &self,
        actor: &CurrentSession,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        validate_password(new_password)?;
        let password_hash = sqlx::query_scalar::<_, String>(
            "SELECT password_hash FROM admin_accounts WHERE id = ?1",
        )
        .bind(actor.account.id)
        .fetch_one(&self.db)
        .await?;
        if !verify_password(current_password, &password_hash)? {
            return Err(AuthError::Unauthorized("当前密码不正确".to_owned()));
        }
        let next_hash = hash_password(new_password)?;
        sqlx::query(
            r#"
            UPDATE admin_accounts
            SET password_hash = ?2,
                password_changed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(actor.account.id)
        .bind(next_hash)
        .execute(&self.db)
        .await?;
        sqlx::query(
            r#"
            UPDATE admin_sessions
            SET session_status = 'revoked',
                revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                revoke_reason = 'password_changed',
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE account_id = ?1
              AND session_status = 'active'
              AND id <> ?2
            "#,
        )
        .bind(actor.account.id)
        .bind(actor.session_id)
        .execute(&self.db)
        .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "profile.password_change",
            target_type: "account",
            target_id: &actor.account.id.to_string(),
            message: "修改个人密码",
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionListItem>, AuthError> {
        sqlx::query_as::<_, SessionListItem>(
            r#"
            SELECT
                s.id,
                s.account_id,
                a.username,
                a.display_name,
                s.session_status,
                s.access_expires_at,
                s.refresh_expires_at,
                s.last_ip,
                s.user_agent,
                s.created_at
            FROM admin_sessions s
            JOIN admin_accounts a ON a.id = s.account_id
            ORDER BY s.id DESC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn list_api_tokens(&self) -> Result<Vec<ApiTokenListItem>, AuthError> {
        sqlx::query_as::<_, ApiTokenListItem>(
            r#"
            SELECT
                t.id,
                t.account_id,
                a.username,
                a.display_name,
                t.token_prefix,
                t.source,
                t.status,
                t.last_used_at,
                t.last_used_ip,
                t.revoked_at,
                t.created_at,
                t.updated_at
            FROM api_tokens t
            JOIN admin_accounts a ON a.id = t.account_id
            ORDER BY t.id DESC
            LIMIT 100
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn create_api_token(
        &self,
        actor: &CurrentSession,
        source: &str,
    ) -> Result<CreatedApiToken, AuthError> {
        let source = normalize_api_token_source(source)?;
        let token = generate_token()?;
        let token_hash = hash_token(&token);
        let token_prefix = token_prefix(&token);
        let token_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO api_tokens(account_id, token_hash, token_prefix, source)
            VALUES (?1, ?2, ?3, ?4)
            RETURNING id
            "#,
        )
        .bind(actor.account.id)
        .bind(&token_hash)
        .bind(&token_prefix)
        .bind(&source)
        .fetch_one(&self.db)
        .await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "api_tokens.create",
            target_type: "api_token",
            target_id: &token_id.to_string(),
            message: &format!("create api token source={source} prefix={token_prefix}"),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(CreatedApiToken {
            id: token_id,
            token,
            token_prefix,
            source,
        })
    }

    pub async fn revoke_api_token(
        &self,
        actor: &CurrentSession,
        token_id: i64,
    ) -> Result<(), AuthError> {
        let row = sqlx::query(
            r#"
            UPDATE api_tokens
            SET status = 'revoked',
                revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
              AND status = 'active'
            RETURNING token_prefix, source
            "#,
        )
        .bind(token_id)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else {
            return Err(AuthError::InvalidInput(
                "API Token 不存在或已吊销".to_owned(),
            ));
        };
        let token_prefix: String = row.get("token_prefix");
        let source: String = row.get("source");
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "api_tokens.revoke",
            target_type: "api_token",
            target_id: &token_id.to_string(),
            message: &format!("revoke api token source={source} prefix={token_prefix}"),
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn authenticate_api_token(
        &self,
        token: &str,
        client_ip: &str,
    ) -> Result<ApiTokenAuthSession, AuthError> {
        let token_hash = hash_token(token);
        let row = sqlx::query(
            r#"
            SELECT
                t.id AS token_id,
                t.source AS token_source,
                a.id,
                a.username,
                a.display_name,
                a.status,
                a.is_super_admin
            FROM api_tokens t
            JOIN admin_accounts a ON a.id = t.account_id
            WHERE t.token_hash = ?1
              AND t.status = 'active'
              AND a.status = 'active'
            "#,
        )
        .bind(&token_hash)
        .fetch_optional(&self.db)
        .await?;
        let Some(row) = row else {
            return Err(AuthError::Unauthorized("API Token 无效或已吊销".to_owned()));
        };
        let account = row_to_account(&row);
        let token_id: i64 = row.get("token_id");
        let token_source: String = row.get("token_source");
        sqlx::query(
            r#"
            UPDATE api_tokens
            SET last_used_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                last_used_ip = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(token_id)
        .bind(client_ip)
        .execute(&self.db)
        .await?;
        let source = token_source.clone();
        Ok(ApiTokenAuthSession {
            session: CurrentSession {
                session_id: 0,
                role_codes: self.role_codes(account.id).await?,
                permission_keys: self.permission_keys(account.id).await?,
                is_super_admin: account.is_super_admin,
                account,
                csrf_token: String::new(),
                access_token_hash: format!("api:{token_id}:{token_source}"),
            },
            token_id,
            source,
        })
    }

    pub async fn revoke_admin_session(
        &self,
        actor: &CurrentSession,
        session_id: i64,
    ) -> Result<(), AuthError> {
        if session_id == actor.session_id {
            return Err(AuthError::InvalidInput(
                "不能在这里强制下线当前会话，请使用退出登录".to_owned(),
            ));
        }
        self.revoke_session_record(session_id, "admin_revoked")
            .await?;
        self.store.delete_session_by_id(session_id).await?;
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action: "rbac.session_revoke",
            target_type: "session",
            target_id: &session_id.to_string(),
            message: "强制下线会话",
            ip: "",
            user_agent: "",
        })
        .await?;
        Ok(())
    }

    pub async fn list_audit_logs(&self) -> Result<Vec<AuditLogItem>, AuthError> {
        self.list_audit_logs_filtered(AuditLogFilter::default())
            .await
    }

    pub async fn list_audit_logs_filtered(
        &self,
        filter: AuditLogFilter,
    ) -> Result<Vec<AuditLogItem>, AuthError> {
        let filter = normalize_audit_filter(filter);
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
            SELECT
                id,
                actor_username,
                action,
                target_type,
                target_id,
                message,
                ip,
                created_at
            FROM admin_audit_logs
            WHERE 1 = 1
            "#,
        );
        push_audit_filter_clauses(&mut builder, &filter);
        builder.push(
            r#"
            ORDER BY id DESC
            LIMIT 100
            "#,
        );
        builder
            .build_query_as::<AuditLogItem>()
            .fetch_all(&self.db)
            .await
            .map_err(AuthError::from)
    }

    pub async fn audit_action_options(&self) -> Result<Vec<AuditFilterOption>, AuthError> {
        sqlx::query_as::<_, AuditFilterOption>(
            r#"
            SELECT DISTINCT action AS value
            FROM admin_audit_logs
            WHERE action != ''
            ORDER BY action
            LIMIT 200
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn audit_target_type_options(&self) -> Result<Vec<AuditFilterOption>, AuthError> {
        sqlx::query_as::<_, AuditFilterOption>(
            r#"
            SELECT DISTINCT target_type AS value
            FROM admin_audit_logs
            WHERE target_type != ''
            ORDER BY target_type
            LIMIT 100
            "#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    pub async fn record_system_audit(
        &self,
        actor: &CurrentSession,
        action: &str,
        target_type: &str,
        target_id: &str,
        message: &str,
    ) -> Result<(), AuthError> {
        self.record_audit(AuditRecord {
            actor_account_id: Some(actor.account.id),
            actor_username: &actor.account.username,
            action,
            target_type,
            target_id,
            message,
            ip: "",
            user_agent: "",
        })
        .await
    }

    async fn revoke_sessions_by_account(
        &self,
        account_id: i64,
        reason: &str,
    ) -> Result<u64, AuthError> {
        let result = sqlx::query(
            r#"
            UPDATE admin_sessions
            SET session_status = 'revoked',
                revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                revoke_reason = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE account_id = ?1
              AND session_status = 'active'
            "#,
        )
        .bind(account_id)
        .bind(reason)
        .execute(&self.db)
        .await?;
        self.store.delete_sessions_by_account(account_id).await?;
        Ok(result.rows_affected())
    }

    async fn revoke_session_record(&self, session_id: i64, reason: &str) -> Result<(), AuthError> {
        sqlx::query(
            r#"
            UPDATE admin_sessions
            SET session_status = 'revoked',
                revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                revoke_reason = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(session_id)
        .bind(reason)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn revoke_sessions_by_role(&self, role_id: i64, reason: &str) -> Result<(), AuthError> {
        let account_ids = sqlx::query_scalar::<_, i64>(
            "SELECT account_id FROM admin_account_roles WHERE role_id = ?1",
        )
        .bind(role_id)
        .fetch_all(&self.db)
        .await?;
        for account_id in account_ids {
            self.revoke_sessions_by_account(account_id, reason).await?;
        }
        Ok(())
    }

    async fn issue_session(
        &self,
        account: AuthAccount,
        client_ip: &str,
        user_agent: &str,
    ) -> Result<AuthResult, AuthError> {
        let role_codes = self.role_codes(account.id).await?;
        let permission_keys = self.permission_keys(account.id).await?;
        let access_token = generate_token()?;
        let refresh_token = generate_token()?;
        let csrf_token = generate_token()?;
        let access_hash = hash_token(&access_token);
        let refresh_hash = hash_token(&refresh_token);
        let session_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO admin_sessions(
                account_id,
                session_status,
                access_token_hash,
                refresh_token_hash,
                access_expires_at,
                refresh_expires_at,
                last_ip,
                user_agent
            )
            VALUES (
                ?1,
                'active',
                ?2,
                ?3,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+2 hours'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+14 days'),
                ?4,
                ?5
            )
            RETURNING id
            "#,
        )
        .bind(account.id)
        .bind(&access_hash)
        .bind(&refresh_hash)
        .bind(client_ip)
        .bind(user_agent)
        .fetch_one(&self.db)
        .await?;
        let snapshot = SessionSnapshot {
            session_id,
            account: account.clone(),
            role_codes: role_codes.clone(),
            permission_keys: permission_keys.clone(),
            is_super_admin: account.is_super_admin,
            access_token_hash: access_hash,
            refresh_token_hash: refresh_hash,
            access_expires_at: timestamp_after(ACCESS_TTL_SECS),
            refresh_expires_at: timestamp_after(REFRESH_TTL_SECS),
            csrf_token,
        };
        self.store
            .save(&snapshot, Duration::from_secs(REFRESH_TTL_SECS as u64))
            .await?;
        Ok(AuthResult {
            session: snapshot.current_session(),
            tokens: SessionTokens {
                access_token,
                refresh_token,
            },
        })
    }

    async fn role_codes(&self, account_id: i64) -> Result<Vec<String>, AuthError> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT r.role_code
            FROM admin_roles r
            JOIN admin_account_roles ar ON ar.role_id = r.id
            WHERE ar.account_id = ?1
              AND r.status = 'active'
            ORDER BY r.role_code
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    async fn permission_keys(&self, account_id: i64) -> Result<Vec<String>, AuthError> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT DISTINCT p.permission_key
            FROM admin_permissions p
            JOIN admin_role_permissions rp ON rp.permission_id = p.id
            JOIN admin_roles r ON r.id = rp.role_id
            JOIN admin_account_roles ar ON ar.role_id = r.id
            WHERE ar.account_id = ?1
              AND r.status = 'active'
            ORDER BY p.permission_key
            "#,
        )
        .bind(account_id)
        .fetch_all(&self.db)
        .await
        .map_err(AuthError::from)
    }

    async fn record_audit(&self, record: AuditRecord<'_>) -> Result<(), AuthError> {
        sqlx::query(
            r#"
            INSERT INTO admin_audit_logs(
                actor_account_id,
                actor_username,
                action,
                target_type,
                target_id,
                message,
                ip,
                user_agent
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
        )
        .bind(record.actor_account_id)
        .bind(record.actor_username)
        .bind(record.action)
        .bind(record.target_type)
        .bind(record.target_id)
        .bind(record.message)
        .bind(record.ip)
        .bind(record.user_agent)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn record_failed_login(
        &self,
        account_id: i64,
        failed_attempts: i64,
        locked: bool,
    ) -> Result<(), AuthError> {
        sqlx::query(
            r#"
            UPDATE admin_accounts
            SET failed_login_attempts = ?2,
                status = CASE WHEN ?3 THEN 'locked' ELSE status END,
                locked_at = CASE
                    WHEN ?3 THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                    ELSE locked_at
                END,
                locked_reason = CASE
                    WHEN ?3 THEN '连续登录失败自动锁定'
                    ELSE locked_reason
                END,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?1
            "#,
        )
        .bind(account_id)
        .bind(failed_attempts)
        .bind(locked)
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

fn normalize_audit_filter(mut filter: AuditLogFilter) -> AuditLogFilter {
    filter.action = normalize_optional_filter(filter.action);
    filter.target_type = normalize_optional_filter(filter.target_type);
    filter.actor =
        normalize_optional_filter(filter.actor).map(|actor| actor.chars().take(80).collect());
    filter.query =
        normalize_optional_filter(filter.query).map(|query| query.chars().take(120).collect());
    filter
}

fn normalize_api_token_source(source: &str) -> Result<String, AuthError> {
    let source = source.trim();
    if source.is_empty() {
        return Err(AuthError::InvalidInput("请输入 Token 来源".to_owned()));
    }
    if source.chars().count() > 80 {
        return Err(AuthError::InvalidInput(
            "Token 来源不能超过 80 个字符".to_owned(),
        ));
    }
    Ok(source.to_owned())
}

fn token_prefix(token: &str) -> String {
    token.chars().take(10).collect()
}

fn normalize_optional_filter(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn push_audit_filter_clauses(builder: &mut QueryBuilder<'_, Sqlite>, filter: &AuditLogFilter) {
    if let Some(action) = &filter.action {
        builder.push(" AND action = ");
        builder.push_bind(action.clone());
    }
    if let Some(target_type) = &filter.target_type {
        builder.push(" AND target_type = ");
        builder.push_bind(target_type.clone());
    }
    if let Some(actor) = &filter.actor {
        let like_actor = format!("%{actor}%");
        builder.push(" AND actor_username LIKE ");
        builder.push_bind(like_actor);
    }
    if let Some(query) = &filter.query {
        let like_query = format!("%{query}%");
        builder.push(" AND (action LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR target_type LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR target_id LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR message LIKE ");
        builder.push_bind(like_query.clone());
        builder.push(" OR actor_username LIKE ");
        builder.push_bind(like_query);
        builder.push(")");
    }
}

fn normalize_username(username: &str) -> Result<String, AuthError> {
    let username = username.trim().to_ascii_lowercase();
    if username.is_empty() {
        return Err(AuthError::InvalidInput("请输入用户名".to_owned()));
    }
    if username.chars().count() < 3 {
        return Err(AuthError::InvalidInput(
            "用户名至少需要 3 个字符".to_owned(),
        ));
    }
    Ok(username)
}

fn normalize_role_code(role_code: &str) -> Result<String, AuthError> {
    let role_code = role_code.trim().to_ascii_lowercase();
    if role_code.is_empty() {
        return Err(AuthError::InvalidInput("请输入角色编码".to_owned()));
    }
    if !role_code
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        return Err(AuthError::InvalidInput(
            "角色编码只能包含小写字母、数字、下划线和中划线".to_owned(),
        ));
    }
    Ok(role_code)
}

async fn replace_account_roles(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: i64,
    role_ids: &[i64],
) -> Result<(), AuthError> {
    sqlx::query("DELETE FROM admin_account_roles WHERE account_id = ?1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    for role_id in unique_ids(role_ids) {
        sqlx::query(
            "INSERT OR IGNORE INTO admin_account_roles(account_id, role_id) VALUES (?1, ?2)",
        )
        .bind(account_id)
        .bind(role_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn replace_role_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    role_id: i64,
    permission_ids: &[i64],
) -> Result<(), AuthError> {
    sqlx::query("DELETE FROM admin_role_permissions WHERE role_id = ?1")
        .bind(role_id)
        .execute(&mut **tx)
        .await?;
    for permission_id in unique_ids(permission_ids) {
        sqlx::query(
            "INSERT OR IGNORE INTO admin_role_permissions(role_id, permission_id) VALUES (?1, ?2)",
        )
        .bind(role_id)
        .bind(permission_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn delete_stale_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), AuthError> {
    let current_permission_keys = all_permissions()
        .iter()
        .map(|permission| permission.key)
        .collect::<std::collections::BTreeSet<_>>();
    let stale_permission_ids = sqlx::query("SELECT id, permission_key FROM admin_permissions")
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .filter_map(|row| {
            let id = row.get::<i64, _>("id");
            let key = row.get::<String, _>("permission_key");
            (!current_permission_keys.contains(key.as_str())).then_some(id)
        })
        .collect::<Vec<_>>();

    for permission_id in stale_permission_ids {
        sqlx::query("DELETE FROM admin_role_permissions WHERE permission_id = ?1")
            .bind(permission_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM admin_permissions WHERE id = ?1")
            .bind(permission_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn sync_builtin_role_permissions(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<(), AuthError> {
    for policy in BUILTIN_ROLE_POLICIES {
        let Some(role_id) =
            sqlx::query_scalar::<_, i64>("SELECT id FROM admin_roles WHERE role_code = ?1")
                .bind(policy.role_code)
                .fetch_optional(&mut **tx)
                .await?
        else {
            continue;
        };

        let permission_ids = match policy.permissions {
            BuiltinPermissionScope::AllCurrent => {
                sqlx::query_scalar::<_, i64>("SELECT id FROM admin_permissions ORDER BY id")
                    .fetch_all(&mut **tx)
                    .await?
            }
            BuiltinPermissionScope::Explicit(permission_keys) => {
                let mut ids = Vec::with_capacity(permission_keys.len());
                for permission_key in permission_keys {
                    if let Some(id) = sqlx::query_scalar::<_, i64>(
                        "SELECT id FROM admin_permissions WHERE permission_key = ?1",
                    )
                    .bind(permission_key)
                    .fetch_optional(&mut **tx)
                    .await?
                    {
                        ids.push(id);
                    }
                }
                ids
            }
        };
        replace_role_permissions(tx, role_id, &permission_ids).await?;
    }
    Ok(())
}

fn unique_ids(ids: &[i64]) -> Vec<i64> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn account_status_name(status: &str) -> &'static str {
    match status {
        "active" => "启用",
        "disabled" => "禁用",
        "locked" => "锁定",
        _ => "未知",
    }
}

fn format_account_label(username: &str, display_name: &str) -> String {
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name == username {
        username.to_owned()
    } else {
        format!("{display_name} ({username})")
    }
}

fn role_summary_from_names(names: Vec<String>) -> String {
    if names.is_empty() {
        "未分配".to_owned()
    } else {
        names.join("、")
    }
}

fn row_to_account(row: &SqliteRow) -> AuthAccount {
    AuthAccount {
        id: row.get("id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        status: row.get("status"),
        is_super_admin: row.get::<i64, _>("is_super_admin") == 1,
    }
}

fn timestamp_after(seconds: i64) -> String {
    // SQLite owns the authoritative expiry check; this string is only cached in memory.
    format!("+{seconds}s")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};

    use super::*;
    use crate::auth::MemorySessionStore;

    async fn auth_service() -> AuthService {
        let db = SqlitePool::connect_with(
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
        let auth = AuthService::new(db, Arc::new(MemorySessionStore::new()));
        auth.sync_permission_registry()
            .await
            .expect("sync permission registry");
        auth
    }

    async fn bootstrap_admin(auth: &AuthService) -> AuthResult {
        auth.bootstrap_init(LoginInput {
            username: "admin".to_owned(),
            password: "password123".to_owned(),
            display_name: Some("管理员".to_owned()),
            client_ip: "127.0.0.1".to_owned(),
            user_agent: "auth-test".to_owned(),
        })
        .await
        .expect("bootstrap admin")
    }

    async fn login_test_account(auth: &AuthService, username: &str) -> AuthResult {
        auth.login(LoginInput {
            username: username.to_owned(),
            password: "password123".to_owned(),
            display_name: None,
            client_ip: "127.0.0.1".to_owned(),
            user_agent: "auth-test".to_owned(),
        })
        .await
        .expect("login test account")
    }

    #[tokio::test]
    async fn bootstrap_always_creates_fixed_admin_account() {
        let auth = auth_service().await;

        let initial = auth
            .bootstrap_init(LoginInput {
                username: "root".to_owned(),
                password: "password123".to_owned(),
                display_name: Some("管理员".to_owned()),
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "auth-test".to_owned(),
            })
            .await
            .expect("bootstrap admin");

        assert_eq!(initial.session.account.username, "admin");
        assert!(initial.session.account.is_super_admin);

        let usernames = sqlx::query_scalar::<_, String>("SELECT username FROM admin_accounts")
            .fetch_all(auth.db())
            .await
            .expect("read account usernames");
        assert_eq!(usernames, vec!["admin".to_owned()]);
    }

    #[tokio::test]
    async fn refresh_rotates_session_when_refresh_token_is_in_store() {
        let auth = auth_service().await;
        let initial = bootstrap_admin(&auth).await;

        let refreshed = auth
            .refresh(&initial.tokens.refresh_token)
            .await
            .expect("refresh token");

        assert_ne!(initial.session.session_id, refreshed.session.session_id);
        assert_ne!(
            initial.tokens.refresh_token, refreshed.tokens.refresh_token,
            "refresh should rotate refresh token"
        );
        assert!(
            auth.refresh(&initial.tokens.refresh_token).await.is_err(),
            "old refresh token should be revoked after rotation"
        );
    }

    #[tokio::test]
    async fn refresh_requires_session_store_entry() {
        let auth = auth_service().await;
        let initial = bootstrap_admin(&auth).await;
        auth.store
            .delete_session_by_id(initial.session.session_id)
            .await
            .expect("delete session from store");

        let err = auth
            .refresh(&initial.tokens.refresh_token)
            .await
            .expect_err("refresh without store session should fail");

        assert!(matches!(err, AuthError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn logout_revokes_access_and_refresh_session_store_indexes() {
        let auth = auth_service().await;
        let initial = bootstrap_admin(&auth).await;

        auth.logout(&initial.session).await.expect("logout session");

        let access_err = auth
            .authenticate_access_token(&initial.tokens.access_token)
            .await
            .expect_err("logged out access token should fail");
        assert!(matches!(access_err, AuthError::Unauthorized(_)));

        let refresh_err = auth
            .refresh(&initial.tokens.refresh_token)
            .await
            .expect_err("logged out refresh token should fail");
        assert!(matches!(refresh_err, AuthError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn account_role_changes_revoke_existing_sessions() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;
        let viewer_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'viewer'")
                .fetch_one(auth.db())
                .await
                .expect("read viewer role");
        let deployer_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'deployer'")
                .fetch_one(auth.db())
                .await
                .expect("read deployer role");
        auth.create_account(
            &admin.session,
            "operator",
            "Operator",
            "password123",
            &[viewer_role_id],
        )
        .await
        .expect("create operator account");
        let operator = login_test_account(&auth, "operator").await;

        auth.update_account_roles(
            &admin.session,
            operator.session.account.id,
            &[deployer_role_id],
        )
        .await
        .expect("update operator roles");

        let access_err = auth
            .authenticate_access_token(&operator.tokens.access_token)
            .await
            .expect_err("role-changed access token should fail");
        assert!(matches!(access_err, AuthError::Unauthorized(_)));

        let refresh_err = auth
            .refresh(&operator.tokens.refresh_token)
            .await
            .expect_err("role-changed refresh token should fail");
        assert!(matches!(refresh_err, AuthError::Unauthorized(_)));

        let revoke_reason: String =
            sqlx::query_scalar("SELECT revoke_reason FROM admin_sessions WHERE id = ?1")
                .bind(operator.session.session_id)
                .fetch_one(auth.db())
                .await
                .expect("read revoked session reason");
        assert_eq!(revoke_reason, "role_changed");

        let audit_message: String = sqlx::query_scalar(
            "SELECT message FROM admin_audit_logs WHERE action = 'rbac.account_roles' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(auth.db())
        .await
        .expect("read role update audit message");
        assert!(
            audit_message.contains("Operator (operator)")
                && audit_message.contains("只读用户")
                && audit_message.contains("部署人员")
                && audit_message.contains("撤销活跃会话 1 个"),
            "audit message should describe account, role diff and revoked sessions: {audit_message}"
        );
    }

    #[tokio::test]
    async fn account_management_audit_messages_include_target_and_session_impact() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;
        let viewer_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'viewer'")
                .fetch_one(auth.db())
                .await
                .expect("read viewer role");
        auth.create_account(
            &admin.session,
            "operator",
            "Operator",
            "password123",
            &[viewer_role_id],
        )
        .await
        .expect("create operator account");
        let operator = login_test_account(&auth, "operator").await;

        let create_message: String = sqlx::query_scalar(
            "SELECT message FROM admin_audit_logs WHERE action = 'rbac.account_create' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(auth.db())
        .await
        .expect("read create audit message");
        assert!(
            create_message.contains("Operator (operator)") && create_message.contains("只读用户"),
            "create audit should include account label and initial roles: {create_message}"
        );

        auth.reset_account_password(
            &admin.session,
            operator.session.account.id,
            "newpassword123",
        )
        .await
        .expect("reset operator password");
        let password_message: String = sqlx::query_scalar(
            "SELECT message FROM admin_audit_logs WHERE action = 'rbac.account_reset_password' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(auth.db())
        .await
        .expect("read password audit message");
        assert!(
            password_message.contains("Operator (operator)")
                && password_message.contains("撤销活跃会话 1 个"),
            "password audit should include account label and revoked sessions: {password_message}"
        );

        auth.set_account_status(&admin.session, operator.session.account.id, "disabled")
            .await
            .expect("disable operator account");
        let status_message: String = sqlx::query_scalar(
            "SELECT message FROM admin_audit_logs WHERE action = 'rbac.account_status' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(auth.db())
        .await
        .expect("read status audit message");
        assert!(
            status_message.contains("Operator (operator)")
                && status_message.contains("状态为禁用")
                && status_message.contains("撤销活跃会话 0 个"),
            "status audit should include account label, status and session impact: {status_message}"
        );
    }

    #[tokio::test]
    async fn role_permission_changes_revoke_assigned_account_sessions() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;
        let dashboard_permission_id: i64 = sqlx::query_scalar(
            "SELECT id FROM admin_permissions WHERE permission_key = 'dashboard.view'",
        )
        .fetch_one(auth.db())
        .await
        .expect("read dashboard permission");
        let apps_permission_id: i64 = sqlx::query_scalar(
            "SELECT id FROM admin_permissions WHERE permission_key = 'apps.view'",
        )
        .fetch_one(auth.db())
        .await
        .expect("read apps permission");
        auth.create_role(
            &admin.session,
            "session_guard",
            "Session Guard",
            "test role",
            &[dashboard_permission_id],
        )
        .await
        .expect("create custom role");
        let custom_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'session_guard'")
                .fetch_one(auth.db())
                .await
                .expect("read custom role");
        auth.create_account(
            &admin.session,
            "operator",
            "Operator",
            "password123",
            &[custom_role_id],
        )
        .await
        .expect("create operator account");
        let operator = login_test_account(&auth, "operator").await;

        auth.update_role_permissions(&admin.session, custom_role_id, &[apps_permission_id])
            .await
            .expect("update custom role permissions");

        let access_err = auth
            .authenticate_access_token(&operator.tokens.access_token)
            .await
            .expect_err("permission-changed access token should fail");
        assert!(matches!(access_err, AuthError::Unauthorized(_)));

        let refresh_err = auth
            .refresh(&operator.tokens.refresh_token)
            .await
            .expect_err("permission-changed refresh token should fail");
        assert!(matches!(refresh_err, AuthError::Unauthorized(_)));

        let revoke_reason: String =
            sqlx::query_scalar("SELECT revoke_reason FROM admin_sessions WHERE id = ?1")
                .bind(operator.session.session_id)
                .fetch_one(auth.db())
                .await
                .expect("read revoked session reason");
        assert_eq!(revoke_reason, "role_permissions_changed");
    }

    #[tokio::test]
    async fn role_permissions_include_required_page_dependencies() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;
        let deploy_permission_id: i64 = sqlx::query_scalar(
            "SELECT id FROM admin_permissions WHERE permission_key = 'services.deploy'",
        )
        .fetch_one(auth.db())
        .await
        .expect("read deploy permission");

        auth.create_role(
            &admin.session,
            "deploy_only",
            "Deploy Only",
            "action dependency test role",
            &[deploy_permission_id],
        )
        .await
        .expect("create role with action permission");

        let keys = role_permission_keys(&auth, "deploy_only").await;
        assert!(keys.contains(&"services.deploy".to_owned()));
        assert!(keys.contains(&"apps.view".to_owned()));
        assert!(keys.contains(&"services.view".to_owned()));
        assert!(keys.contains(&"tasks.view".to_owned()));
    }

    #[tokio::test]
    async fn account_requires_at_least_one_valid_active_role() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;

        let empty_err = auth
            .create_account(&admin.session, "operator", "Operator", "password123", &[])
            .await
            .expect_err("empty roles should fail");
        assert!(matches!(empty_err, AuthError::InvalidInput(_)));

        let invalid_err = auth
            .create_account(
                &admin.session,
                "operator",
                "Operator",
                "password123",
                &[9999],
            )
            .await
            .expect_err("invalid role id should fail");
        assert!(matches!(invalid_err, AuthError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn login_failures_lock_account_until_admin_reactivates_it() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;
        let viewer_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'viewer'")
                .fetch_one(auth.db())
                .await
                .expect("read viewer role");
        auth.create_account(
            &admin.session,
            "operator",
            "Operator",
            "password123",
            &[viewer_role_id],
        )
        .await
        .expect("create account");

        for _ in 0..MAX_FAILED_LOGIN_ATTEMPTS {
            let err = auth
                .login(LoginInput {
                    username: "operator".to_owned(),
                    password: "wrong-password".to_owned(),
                    display_name: None,
                    client_ip: "127.0.0.1".to_owned(),
                    user_agent: "auth-test".to_owned(),
                })
                .await
                .expect_err("wrong password should fail");
            assert!(matches!(err, AuthError::Unauthorized(_)));
        }

        let locked_status: String =
            sqlx::query_scalar("SELECT status FROM admin_accounts WHERE username = 'operator'")
                .fetch_one(auth.db())
                .await
                .expect("read locked status");
        assert_eq!(locked_status, "locked");

        let err = auth
            .login(LoginInput {
                username: "operator".to_owned(),
                password: "password123".to_owned(),
                display_name: None,
                client_ip: "127.0.0.1".to_owned(),
                user_agent: "auth-test".to_owned(),
            })
            .await
            .expect_err("locked account should not login");
        assert!(matches!(err, AuthError::Forbidden(_)));

        let operator_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_accounts WHERE username = 'operator'")
                .fetch_one(auth.db())
                .await
                .expect("read operator id");
        auth.set_account_status(&admin.session, operator_id, "active")
            .await
            .expect("reactivate account");

        let attempts_after_unlock: i64 =
            sqlx::query_scalar("SELECT failed_login_attempts FROM admin_accounts WHERE id = ?1")
                .bind(operator_id)
                .fetch_one(auth.db())
                .await
                .expect("read failed attempts");
        assert_eq!(attempts_after_unlock, 0);

        auth.login(LoginInput {
            username: "operator".to_owned(),
            password: "password123".to_owned(),
            display_name: None,
            client_ip: "127.0.0.1".to_owned(),
            user_agent: "auth-test".to_owned(),
        })
        .await
        .expect("reactivated account should login");
    }

    #[tokio::test]
    async fn admin_cannot_disable_or_remove_roles_from_current_account() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;

        let disable_err = auth
            .set_account_status(&admin.session, admin.session.account.id, "disabled")
            .await
            .expect_err("self disable should fail");
        assert!(matches!(disable_err, AuthError::InvalidInput(_)));

        let lock_err = auth
            .set_account_status(&admin.session, admin.session.account.id, "locked")
            .await
            .expect_err("self lock should fail");
        assert!(matches!(lock_err, AuthError::InvalidInput(_)));

        let role_err = auth
            .update_account_roles(&admin.session, admin.session.account.id, &[])
            .await
            .expect_err("self role update should fail");
        assert!(matches!(role_err, AuthError::InvalidInput(_)));

        let password_err = auth
            .reset_account_password(&admin.session, admin.session.account.id, "newpassword123")
            .await
            .expect_err("self password reset should fail");
        assert!(matches!(password_err, AuthError::InvalidInput(_)));

        let status: String = sqlx::query_scalar("SELECT status FROM admin_accounts WHERE id = ?1")
            .bind(admin.session.account.id)
            .fetch_one(auth.db())
            .await
            .expect("read admin status");
        assert_eq!(status, "active");
    }

    #[tokio::test]
    async fn system_roles_cannot_be_disabled_or_rewritten() {
        let auth = auth_service().await;
        let admin = bootstrap_admin(&auth).await;
        let super_admin_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'super_admin'")
                .fetch_one(auth.db())
                .await
                .expect("read super admin role");

        let status_err = auth
            .set_role_status(&admin.session, super_admin_role_id, "disabled")
            .await
            .expect_err("system role status update should fail");
        assert!(matches!(status_err, AuthError::InvalidInput(_)));

        let permission_err = auth
            .update_role_permissions(&admin.session, super_admin_role_id, &[])
            .await
            .expect_err("system role permission update should fail");
        assert!(matches!(permission_err, AuthError::InvalidInput(_)));

        let still_active: String =
            sqlx::query_scalar("SELECT status FROM admin_roles WHERE id = ?1")
                .bind(super_admin_role_id)
                .fetch_one(auth.db())
                .await
                .expect("read super admin role status");
        assert_eq!(still_active, "active");

        let permission_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM admin_role_permissions WHERE role_id = ?1")
                .bind(super_admin_role_id)
                .fetch_one(auth.db())
                .await
                .expect("read super admin permission count");
        assert!(permission_count > 0);
    }

    #[tokio::test]
    async fn sync_permission_registry_repairs_builtin_role_permissions() {
        let auth = auth_service().await;
        let super_admin_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'super_admin'")
                .fetch_one(auth.db())
                .await
                .expect("read super admin role");
        let viewer_role_id: i64 =
            sqlx::query_scalar("SELECT id FROM admin_roles WHERE role_code = 'viewer'")
                .fetch_one(auth.db())
                .await
                .expect("read viewer role");
        let deploy_permission_id: i64 = sqlx::query_scalar(
            "SELECT id FROM admin_permissions WHERE permission_key = 'services.deploy'",
        )
        .fetch_one(auth.db())
        .await
        .expect("read deploy permission");

        sqlx::query("DELETE FROM admin_role_permissions WHERE role_id IN (?1, ?2)")
            .bind(super_admin_role_id)
            .bind(viewer_role_id)
            .execute(auth.db())
            .await
            .expect("delete builtin role permissions");
        sqlx::query("INSERT INTO admin_role_permissions(role_id, permission_id) VALUES (?1, ?2)")
            .bind(viewer_role_id)
            .bind(deploy_permission_id)
            .execute(auth.db())
            .await
            .expect("poison viewer permissions");

        auth.sync_permission_registry()
            .await
            .expect("sync permission registry");

        let total_permission_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM admin_permissions")
                .fetch_one(auth.db())
                .await
                .expect("count all permissions");
        let super_admin_permission_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM admin_role_permissions WHERE role_id = ?1")
                .bind(super_admin_role_id)
                .fetch_one(auth.db())
                .await
                .expect("count super admin permissions");
        assert_eq!(super_admin_permission_count, total_permission_count);

        let viewer_permission_keys = sqlx::query_scalar::<_, String>(
            r#"
            SELECT p.permission_key
            FROM admin_permissions p
            JOIN admin_role_permissions rp ON rp.permission_id = p.id
            WHERE rp.role_id = ?1
            ORDER BY p.permission_key
            "#,
        )
        .bind(viewer_role_id)
        .fetch_all(auth.db())
        .await
        .expect("read viewer permission keys");
        let mut expected_viewer_keys: Vec<String> = VIEWER_PERMISSION_KEYS
            .iter()
            .map(|permission| permission.to_string())
            .collect();
        expected_viewer_keys.sort();
        assert_eq!(viewer_permission_keys, expected_viewer_keys);
        assert!(
            !viewer_permission_keys
                .iter()
                .any(|permission| permission == "services.deploy"),
            "viewer permissions should be reset to the version-owned policy"
        );
    }

    #[tokio::test]
    async fn builtin_roles_use_granular_action_permissions() {
        let auth = auth_service().await;

        let deployer_permission_keys = role_permission_keys(&auth, "deployer").await;
        assert!(deployer_permission_keys.contains(&"tasks.retry".to_owned()));
        assert!(!deployer_permission_keys.contains(&"artifacts.upload".to_owned()));
        assert!(!deployer_permission_keys.contains(&"apps.status".to_owned()));

        let operator_permission_keys = role_permission_keys(&auth, "operator").await;
        assert!(operator_permission_keys.contains(&"apps.status".to_owned()));
        assert!(operator_permission_keys.contains(&"artifacts.upload".to_owned()));
        assert!(operator_permission_keys.contains(&"nodes.install".to_owned()));
        assert!(!operator_permission_keys.contains(&"tasks.retry".to_owned()));
        assert!(!operator_permission_keys.contains(&"services.deploy".to_owned()));
        assert!(!operator_permission_keys.contains(&"settings.update".to_owned()));

        let viewer_permission_keys = role_permission_keys(&auth, "viewer").await;
        assert!(!viewer_permission_keys.contains(&"apps.status".to_owned()));
        assert!(!viewer_permission_keys.contains(&"tasks.retry".to_owned()));
        assert!(!viewer_permission_keys.contains(&"artifacts.upload".to_owned()));
        assert!(!viewer_permission_keys.contains(&"nodes.install".to_owned()));
        assert!(!viewer_permission_keys.contains(&"settings.update".to_owned()));

        let legacy_permission_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM admin_permissions WHERE permission_key = 'apps.delete'",
        )
        .fetch_one(auth.db())
        .await
        .expect("count legacy app delete permission");
        assert_eq!(legacy_permission_count, 0);
    }

    async fn role_permission_keys(auth: &AuthService, role_code: &str) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT p.permission_key
            FROM admin_permissions p
            JOIN admin_role_permissions rp ON rp.permission_id = p.id
            JOIN admin_roles r ON r.id = rp.role_id
            WHERE r.role_code = ?1
            ORDER BY p.permission_key
            "#,
        )
        .bind(role_code)
        .fetch_all(auth.db())
        .await
        .expect("read role permission keys")
    }
}
