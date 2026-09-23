//! Refresh-token storage. Production: Secret Service (KWallet on Plasma). Tests: memory.

use std::collections::HashMap;
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

#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Whether a refresh token is stored. Must not require unlocking the wallet.
    async fn exists(&self) -> Result<bool, SecretError>;
    /// Reads the refresh token; the wallet may prompt the user to unlock it.
    async fn load(&self) -> Result<Option<String>, SecretError>;
    async fn store(&self, refresh_token: &str) -> Result<(), SecretError>;
    async fn delete(&self) -> Result<(), SecretError>;
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore {
    token: Mutex<Option<String>>,
    locked: Mutex<bool>,
}

impl MemoryStore {
    pub fn with_token(token: &str) -> Self {
        let store = Self::default();
        *store.token.lock().unwrap() = Some(token.to_owned());
        store
    }

    /// Simulates a locked wallet whose unlock prompt the user refuses.
    pub fn set_locked(&self, locked: bool) {
        *self.locked.lock().unwrap() = locked;
    }

    pub fn current(&self) -> Option<String> {
        self.token.lock().unwrap().clone()
    }
}

#[async_trait]
impl SecretStore for MemoryStore {
    async fn exists(&self) -> Result<bool, SecretError> {
        Ok(self.token.lock().unwrap().is_some())
    }

    async fn load(&self) -> Result<Option<String>, SecretError> {
        if *self.locked.lock().unwrap() {
            return Err(SecretError::Locked);
        }
        Ok(self.token.lock().unwrap().clone())
    }

    async fn store(&self, refresh_token: &str) -> Result<(), SecretError> {
        *self.token.lock().unwrap() = Some(refresh_token.to_owned());
        Ok(())
    }

    async fn delete(&self) -> Result<(), SecretError> {
        *self.token.lock().unwrap() = None;
        Ok(())
    }
}

const LABEL: &str = "KOneDrive refresh token";

/// Secret Service over D-Bus. Deliberately has no file fallback.
pub struct SecretServiceStore {
    kind: &'static str,
}

impl SecretServiceStore {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::with_kind("refresh-token")
    }

    /// A different `kind` attribute keeps test items apart from the real token.
    pub fn with_kind(kind: &'static str) -> Self {
        Self { kind }
    }

    fn attributes(&self) -> HashMap<&'static str, &'static str> {
        HashMap::from([("application", "konedrive"), ("kind", self.kind)])
    }
}

async fn connect() -> Result<oo7::dbus::Service, SecretError> {
    oo7::dbus::Service::new()
        .await
        .map_err(|e| SecretError::Unavailable(e.to_string()))
}

fn other(error: impl std::fmt::Display) -> SecretError {
    SecretError::Other(error.to_string())
}

#[async_trait]
impl SecretStore for SecretServiceStore {
    async fn exists(&self) -> Result<bool, SecretError> {
        // Service-level search (unlike `Collection::search_items`) reports locked items
        // too, so presence can be checked without unlocking the wallet. But it also
        // searches every collection, while `load()`/`delete()` only ever look at the
        // default one: an item anywhere else would make this report signed-in for
        // something `load()` can never read and `delete()` can never remove. So filter
        // the results down to items whose object path lives under the default collection
        // (found through the "default" alias, same one `load()`/`delete()` use via
        // `Service::default_collection`), without unlocking or prompting.
        let connection = zbus::Connection::session()
            .await
            .map_err(|e| SecretError::Unavailable(e.to_string()))?;
        let service = oo7::dbus::api::Service::new(&connection).await.map_err(other)?;
        let Some(default_collection) = service.read_alias("default").await.map_err(other)? else {
            return Ok(false);
        };
        let default_prefix = format!("{}/", default_collection.inner().path());
        let (unlocked, locked) = service.search_items(&self.attributes()).await.map_err(other)?;
        Ok(unlocked
            .iter()
            .chain(locked.iter())
            .any(|item| item.inner().path().as_str().starts_with(&default_prefix)))
    }

    async fn load(&self) -> Result<Option<String>, SecretError> {
        let service = connect().await?;
        let collection = service.default_collection().await.map_err(other)?;
        if collection.is_locked().await.map_err(other)? {
            collection.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        let items = collection.search_items(&self.attributes()).await.map_err(other)?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        if item.is_locked().await.map_err(other)? {
            item.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        let secret = item.secret().await.map_err(other)?;
        String::from_utf8(secret.as_bytes().to_vec()).map(Some).map_err(other)
    }

    async fn store(&self, refresh_token: &str) -> Result<(), SecretError> {
        let service = connect().await?;
        let collection = service.default_collection().await.map_err(other)?;
        if collection.is_locked().await.map_err(other)? {
            collection.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        collection
            .create_item(LABEL, &self.attributes(), refresh_token, true, None)
            .await
            .map_err(other)?;
        Ok(())
    }

    async fn delete(&self) -> Result<(), SecretError> {
        let service = connect().await?;
        let collection = service.default_collection().await.map_err(other)?;
        if collection.is_locked().await.map_err(other)? {
            collection.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        for item in collection.search_items(&self.attributes()).await.map_err(other)? {
            item.delete(None).await.map_err(other)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_round_trip() {
        let store = MemoryStore::default();
        assert!(!store.exists().await.unwrap());
        assert_eq!(store.load().await.unwrap(), None);
        store.store("RT").await.unwrap();
        assert!(store.exists().await.unwrap());
        assert_eq!(store.load().await.unwrap().as_deref(), Some("RT"));
        store.delete().await.unwrap();
        assert!(!store.exists().await.unwrap());
    }

    #[tokio::test]
    async fn locked_store_reports_existence_but_refuses_load() {
        let store = MemoryStore::with_token("RT");
        store.set_locked(true);
        assert!(store.exists().await.unwrap());
        assert!(matches!(store.load().await, Err(SecretError::Locked)));
    }

    /// Uses the real Secret Service (KWallet) with a test-only attribute.
    /// Run explicitly: `cargo test -p konedrived secret_service_round_trip -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn secret_service_round_trip() {
        let store = SecretServiceStore::with_kind("test-refresh-token");
        store.store("test-value").await.unwrap();
        assert!(store.exists().await.unwrap());
        assert_eq!(store.load().await.unwrap().as_deref(), Some("test-value"));
        store.delete().await.unwrap();
        assert!(!store.exists().await.unwrap());
    }
}
