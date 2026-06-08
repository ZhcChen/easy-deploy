use std::{collections::HashMap, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::service::{AuthAccount, AuthError, CurrentSession};

pub type DynSessionStore = Arc<dyn SessionStore>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: i64,
    pub account: AuthAccount,
    pub role_codes: Vec<String>,
    pub permission_keys: Vec<String>,
    pub is_super_admin: bool,
    pub access_token_hash: String,
    pub refresh_token_hash: String,
    pub access_expires_at: String,
    pub refresh_expires_at: String,
    pub csrf_token: String,
}

impl SessionSnapshot {
    pub fn current_session(&self) -> CurrentSession {
        CurrentSession {
            session_id: self.session_id,
            account: self.account.clone(),
            role_codes: self.role_codes.clone(),
            permission_keys: self.permission_keys.clone(),
            is_super_admin: self.is_super_admin,
            csrf_token: self.csrf_token.clone(),
            access_token_hash: self.access_token_hash.clone(),
        }
    }
}

#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn save(&self, session: &SessionSnapshot, ttl: Duration) -> Result<(), AuthError>;
    async fn get_by_access_hash(
        &self,
        access_token_hash: &str,
    ) -> Result<Option<SessionSnapshot>, AuthError>;
    async fn get_by_refresh_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<SessionSnapshot>, AuthError>;
    async fn delete_session(&self, session: &SessionSnapshot) -> Result<(), AuthError>;
    async fn delete_session_by_id(&self, session_id: i64) -> Result<(), AuthError>;
    async fn delete_sessions_by_account(&self, account_id: i64) -> Result<(), AuthError>;
}

#[derive(Default)]
pub struct MemorySessionStore {
    access_index: RwLock<HashMap<String, i64>>,
    refresh_index: RwLock<HashMap<String, i64>>,
    sessions: RwLock<HashMap<i64, SessionSnapshot>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SessionStore for MemorySessionStore {
    async fn save(&self, session: &SessionSnapshot, _ttl: Duration) -> Result<(), AuthError> {
        self.access_index
            .write()
            .await
            .insert(session.access_token_hash.clone(), session.session_id);
        self.refresh_index
            .write()
            .await
            .insert(session.refresh_token_hash.clone(), session.session_id);
        self.sessions
            .write()
            .await
            .insert(session.session_id, session.clone());
        Ok(())
    }

    async fn get_by_access_hash(
        &self,
        access_token_hash: &str,
    ) -> Result<Option<SessionSnapshot>, AuthError> {
        let session_id = self
            .access_index
            .read()
            .await
            .get(access_token_hash)
            .copied();
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        Ok(self.sessions.read().await.get(&session_id).cloned())
    }

    async fn get_by_refresh_hash(
        &self,
        refresh_token_hash: &str,
    ) -> Result<Option<SessionSnapshot>, AuthError> {
        let session_id = self
            .refresh_index
            .read()
            .await
            .get(refresh_token_hash)
            .copied();
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        Ok(self.sessions.read().await.get(&session_id).cloned())
    }

    async fn delete_session(&self, session: &SessionSnapshot) -> Result<(), AuthError> {
        self.access_index
            .write()
            .await
            .remove(&session.access_token_hash);
        self.refresh_index
            .write()
            .await
            .remove(&session.refresh_token_hash);
        self.sessions.write().await.remove(&session.session_id);
        Ok(())
    }

    async fn delete_session_by_id(&self, session_id: i64) -> Result<(), AuthError> {
        let removed = self.sessions.write().await.remove(&session_id);
        if let Some(session) = removed {
            self.access_index
                .write()
                .await
                .remove(&session.access_token_hash);
            self.refresh_index
                .write()
                .await
                .remove(&session.refresh_token_hash);
        }
        Ok(())
    }

    async fn delete_sessions_by_account(&self, account_id: i64) -> Result<(), AuthError> {
        let mut sessions = self.sessions.write().await;
        let removed: Vec<SessionSnapshot> = sessions
            .values()
            .filter(|session| session.account.id == account_id)
            .cloned()
            .collect();
        for session in &removed {
            sessions.remove(&session.session_id);
        }
        drop(sessions);

        let mut access_index = self.access_index.write().await;
        let mut refresh_index = self.refresh_index.write().await;
        for session in removed {
            access_index.remove(&session.access_token_hash);
            refresh_index.remove(&session.refresh_token_hash);
        }
        Ok(())
    }
}
