//! The refresh token's store, as the token manager sees it. The daemon keeps it in the wallet;
//! tests keep it in memory.

#[cfg(any(test, feature = "testing"))]
use std::sync::Mutex;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// No Secret Service provider on the session bus.
    #[error("no secret storage available: {0}")]
    Unavailable(String),
    /// The wallet is locked and the user did not unlock it.
    #[error("secret storage is locked")]
    Locked,
    #[error("secret storage error: {0}")]
    Other(String),
}

/// One account's refresh token, as its services see it.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Whether a refresh token is stored. Must not require unlocking the wallet.
    async fn exists(&self) -> Result<bool, SecretError>;
    /// Reads the refresh token; the wallet may prompt the user to unlock it.
    async fn load(&self) -> Result<Option<String>, SecretError>;
    async fn store(&self, refresh_token: &str) -> Result<(), SecretError>;
    async fn delete(&self) -> Result<(), SecretError>;
    /// Who the token belongs to, for the label of the wallet item stored from now on. Does
    /// nothing by default.
    fn describe(&self, _email: &str) {}
}

/// In-memory store of one token, for tests.
#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub struct MemoryStore {
    token: Mutex<Option<String>>,
    locked: Mutex<bool>,
}

#[cfg(any(test, feature = "testing"))]
impl MemoryStore {
    pub fn with_token(token: &str) -> Self {
        let store = Self::default();
        *crate::lock(&store.token) = Some(token.to_owned());
        store
    }

    /// Simulates a locked wallet whose unlock prompt the user refuses.
    pub fn set_locked(&self, locked: bool) {
        *crate::lock(&self.locked) = locked;
    }

    pub fn current(&self) -> Option<String> {
        crate::lock(&self.token).clone()
    }
}

#[cfg(any(test, feature = "testing"))]
#[async_trait]
impl SecretStore for MemoryStore {
    async fn exists(&self) -> Result<bool, SecretError> {
        Ok(crate::lock(&self.token).is_some())
    }

    async fn load(&self) -> Result<Option<String>, SecretError> {
        if *crate::lock(&self.locked) {
            return Err(SecretError::Locked);
        }
        Ok(crate::lock(&self.token).clone())
    }

    async fn store(&self, refresh_token: &str) -> Result<(), SecretError> {
        *crate::lock(&self.token) = Some(refresh_token.to_owned());
        Ok(())
    }

    async fn delete(&self) -> Result<(), SecretError> {
        *crate::lock(&self.token) = None;
        Ok(())
    }
}
