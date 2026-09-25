//! Refresh-token storage. Production: Secret Service (KWallet on Plasma). Tests: memory.
//!
//! Each account keeps its token in an item of its own ([`Slot::Account`]); version 1's one
//! item ([`Slot::V1`]) is moved into the migrated account's the first time its token is
//! loaded ([`AccountSecrets`], design §7.4).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::config::{ConfigError, ConfigStore};

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

/// Where a refresh token is kept in the wallet.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Version 1's one item: `application=konedrive, kind=refresh-token`.
    V1,
    /// An account's own: `application=konedrive, kind=account-refresh-token, account=<id>`.
    /// A `kind` of its own, because a search matches every item whose attributes *include*
    /// the ones asked for: a search for version 1's item would otherwise find these too.
    Account(String),
}

/// The wallet, item by item.
#[async_trait]
pub trait Wallet: Send + Sync {
    /// Whether the item exists. Must not require unlocking the wallet.
    async fn exists(&self, slot: &Slot) -> Result<bool, SecretError>;
    async fn load(&self, slot: &Slot) -> Result<Option<String>, SecretError>;
    async fn store(&self, slot: &Slot, label: &str, secret: &str) -> Result<(), SecretError>;
    async fn delete(&self, slot: &Slot) -> Result<(), SecretError>;
}

/// A wallet in memory, for tests: each slot's label and secret.
#[derive(Default)]
pub struct MemoryWallet {
    items: Mutex<HashMap<Slot, (String, String)>>,
    locked: Mutex<bool>,
}

impl MemoryWallet {
    /// A wallet holding version 1's item, as the single-account daemon left it.
    pub fn with_v1(token: &str) -> Self {
        let wallet = Self::default();
        wallet.items.lock().unwrap().insert(Slot::V1, (V1_LABEL.into(), token.to_owned()));
        wallet
    }

    /// Simulates a locked wallet whose unlock prompt the user refuses.
    pub fn set_locked(&self, locked: bool) {
        *self.locked.lock().unwrap() = locked;
    }

    pub fn current(&self, slot: &Slot) -> Option<String> {
        self.items.lock().unwrap().get(slot).map(|(_, secret)| secret.clone())
    }

    pub fn label(&self, slot: &Slot) -> Option<String> {
        self.items.lock().unwrap().get(slot).map(|(label, _)| label.clone())
    }
}

#[async_trait]
impl Wallet for MemoryWallet {
    async fn exists(&self, slot: &Slot) -> Result<bool, SecretError> {
        Ok(self.items.lock().unwrap().contains_key(slot))
    }

    async fn load(&self, slot: &Slot) -> Result<Option<String>, SecretError> {
        if *self.locked.lock().unwrap() {
            return Err(SecretError::Locked);
        }
        Ok(self.current(slot))
    }

    async fn store(&self, slot: &Slot, label: &str, secret: &str) -> Result<(), SecretError> {
        if *self.locked.lock().unwrap() {
            return Err(SecretError::Locked);
        }
        self.items.lock().unwrap().insert(slot.clone(), (label.to_owned(), secret.to_owned()));
        Ok(())
    }

    async fn delete(&self, slot: &Slot) -> Result<(), SecretError> {
        self.items.lock().unwrap().remove(slot);
        Ok(())
    }
}

/// The label version 1 gave its item, and an account's before its email is known.
const V1_LABEL: &str = "KOneDrive refresh token";

/// One account's refresh token (design §7.4): its own item, and — while the account's
/// `legacy_token` is set in `config.toml` — version 1's too.
///
/// - [`load`](SecretStore::load) prefers the account's own item; without one it moves
///   version 1's into it (stored under the new attributes, then the old one deleted). Either
///   way the old item is deleted and the flag cleared. The move happens inside the first
///   token refresh, which reads the wallet anyway, so it adds no unlock prompt.
/// - [`exists`](SecretStore::exists) sees either item.
/// - [`delete`](SecretStore::delete) deletes both.
///
/// A crash between storing the new item and deleting the old leaves both; the next load
/// prefers the new one and deletes the old.
pub struct AccountSecrets {
    wallet: Arc<dyn Wallet>,
    config: Arc<ConfigStore>,
    id: String,
    /// The email the token belongs to, for the item's label; empty until known.
    email: Mutex<String>,
}

impl AccountSecrets {
    pub fn new(wallet: Arc<dyn Wallet>, config: Arc<ConfigStore>, id: &str) -> Self {
        Self { wallet, config, id: id.to_owned(), email: Mutex::new(String::new()) }
    }

    fn slot(&self) -> Slot {
        Slot::Account(self.id.clone())
    }

    fn label(&self) -> String {
        let email = self.email.lock().unwrap();
        if email.is_empty() {
            V1_LABEL.into()
        } else {
            format!("KOneDrive: {email}")
        }
    }

    fn legacy(&self) -> bool {
        self.config.account(&self.id).is_some_and(|a| a.legacy_token)
    }

    /// Deletes version 1's item, then clears the flag. A flag that stays set only makes the
    /// next load look for the old item again.
    async fn drop_legacy(&self) {
        if let Err(e) = self.wallet.delete(&Slot::V1).await {
            tracing::warn!("cannot delete the refresh token of version 1: {e}; it is tried again at the next load");
            return;
        }
        let cleared = self.config.update_account(&self.id, |account| {
            account.legacy_token = false;
            Ok::<_, ConfigError>(())
        });
        if let Err(e) = cleared {
            tracing::warn!("cannot record that the refresh token of version 1 was moved: {e}");
        }
    }
}

#[async_trait]
impl SecretStore for AccountSecrets {
    async fn exists(&self) -> Result<bool, SecretError> {
        if self.wallet.exists(&self.slot()).await? {
            return Ok(true);
        }
        if self.legacy() {
            return self.wallet.exists(&Slot::V1).await;
        }
        Ok(false)
    }

    async fn load(&self) -> Result<Option<String>, SecretError> {
        let own = self.wallet.load(&self.slot()).await?;
        if !self.legacy() {
            return Ok(own);
        }
        let token = match own {
            Some(token) => Some(token),
            None => match self.wallet.load(&Slot::V1).await? {
                Some(token) => {
                    self.wallet.store(&self.slot(), &self.label(), &token).await?;
                    tracing::info!("the refresh token of version 1 is now account {}'s", self.id);
                    Some(token)
                }
                None => None,
            },
        };
        self.drop_legacy().await;
        Ok(token)
    }

    async fn store(&self, refresh_token: &str) -> Result<(), SecretError> {
        self.wallet.store(&self.slot(), &self.label(), refresh_token).await
    }

    async fn delete(&self) -> Result<(), SecretError> {
        self.wallet.delete(&self.slot()).await?;
        if self.legacy() {
            self.drop_legacy().await;
        }
        Ok(())
    }

    fn describe(&self, email: &str) {
        *self.email.lock().unwrap() = email.to_owned();
    }
}

/// Secret Service over D-Bus. Deliberately has no file fallback.
pub struct SecretServiceWallet {
    /// Version 1's `kind`.
    v1_kind: &'static str,
    /// Every account's `kind`.
    account_kind: &'static str,
}

impl SecretServiceWallet {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { v1_kind: "refresh-token", account_kind: "account-refresh-token" }
    }

    /// Other `kind` attributes keep test items apart from the real tokens.
    pub fn for_tests() -> Self {
        Self { v1_kind: "test-refresh-token", account_kind: "test-account-refresh-token" }
    }

    fn attributes(&self, slot: &Slot) -> HashMap<&'static str, String> {
        let mut attributes = HashMap::from([("application", "konedrive".to_owned())]);
        match slot {
            Slot::V1 => {
                attributes.insert("kind", self.v1_kind.to_owned());
            }
            Slot::Account(id) => {
                attributes.insert("kind", self.account_kind.to_owned());
                attributes.insert("account", id.clone());
            }
        }
        attributes
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
impl Wallet for SecretServiceWallet {
    async fn exists(&self, slot: &Slot) -> Result<bool, SecretError> {
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
        let (unlocked, locked) = service.search_items(&self.attributes(slot)).await.map_err(other)?;
        Ok(unlocked
            .iter()
            .chain(locked.iter())
            .any(|item| item.inner().path().as_str().starts_with(&default_prefix)))
    }

    async fn load(&self, slot: &Slot) -> Result<Option<String>, SecretError> {
        let service = connect().await?;
        let collection = service.default_collection().await.map_err(other)?;
        if collection.is_locked().await.map_err(other)? {
            collection.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        let items = collection.search_items(&self.attributes(slot)).await.map_err(other)?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        if item.is_locked().await.map_err(other)? {
            item.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        let secret = item.secret().await.map_err(other)?;
        String::from_utf8(secret.as_bytes().to_vec()).map(Some).map_err(other)
    }

    async fn store(&self, slot: &Slot, label: &str, secret: &str) -> Result<(), SecretError> {
        let service = connect().await?;
        let collection = service.default_collection().await.map_err(other)?;
        if collection.is_locked().await.map_err(other)? {
            collection.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        collection
            .create_item(label, &self.attributes(slot), secret, true, None)
            .await
            .map_err(other)?;
        Ok(())
    }

    async fn delete(&self, slot: &Slot) -> Result<(), SecretError> {
        let service = connect().await?;
        let collection = service.default_collection().await.map_err(other)?;
        if collection.is_locked().await.map_err(other)? {
            collection.unlock(None).await.map_err(|_| SecretError::Locked)?;
        }
        for item in collection.search_items(&self.attributes(slot)).await.map_err(other)? {
            item.delete(None).await.map_err(other)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

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

    /// A configuration with one account whose `legacy_token` is set, its id, and the
    /// account's secrets on `wallet`.
    async fn migrated(dir: &std::path::Path, wallet: &Arc<MemoryWallet>) -> (Arc<ConfigStore>, String, AccountSecrets) {
        let config = Arc::new(ConfigStore::open(&Paths::in_dir(dir), async { false }).await);
        let id = config.add_account("Personal").unwrap().id;
        config
            .update_account(&id, |a| {
                a.legacy_token = true;
                Ok::<_, ConfigError>(())
            })
            .unwrap();
        let secrets = AccountSecrets::new(Arc::clone(wallet) as Arc<dyn Wallet>, Arc::clone(&config), &id);
        (config, id, secrets)
    }

    /// Design §7.4 (test 3): version 1's item counts before the move, moves into the
    /// account's own item on the first load and is deleted, and the flag is cleared.
    #[tokio::test]
    async fn the_token_of_version_1_moves_into_the_account_on_the_first_load() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(MemoryWallet::with_v1("RT-OLD"));
        let (config, id, secrets) = migrated(dir.path(), &wallet).await;
        let own = Slot::Account(id.clone());
        assert!(secrets.exists().await.unwrap(), "the old item counts while the flag is set");

        secrets.describe("ann@outlook.com");
        assert_eq!(secrets.load().await.unwrap().as_deref(), Some("RT-OLD"));

        assert_eq!(wallet.current(&own).as_deref(), Some("RT-OLD"));
        assert_eq!(wallet.label(&own).as_deref(), Some("KOneDrive: ann@outlook.com"));
        assert_eq!(wallet.current(&Slot::V1), None, "the old item is deleted once moved");
        assert!(!config.account(&id).unwrap().legacy_token, "and the flag is cleared");
        assert_eq!(secrets.load().await.unwrap().as_deref(), Some("RT-OLD"));

        // With the flag cleared, an item of version 1 is not the account's any more.
        wallet.store(&Slot::V1, V1_LABEL, "RT-STRAY").await.unwrap();
        wallet.delete(&own).await.unwrap();
        assert!(!secrets.exists().await.unwrap());
    }

    /// A crash between storing the new item and deleting the old leaves both: the next load
    /// prefers the account's own, and deletes the old.
    #[tokio::test]
    async fn with_both_items_the_accounts_own_wins_and_the_old_one_goes() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(MemoryWallet::with_v1("RT-OLD"));
        let (config, id, secrets) = migrated(dir.path(), &wallet).await;
        wallet.store(&Slot::Account(id.clone()), "x", "RT-NEW").await.unwrap();

        assert_eq!(secrets.load().await.unwrap().as_deref(), Some("RT-NEW"));
        assert_eq!(wallet.current(&Slot::V1), None);
        assert!(!config.account(&id).unwrap().legacy_token);
    }

    /// A sign-out while the flag is set deletes both items.
    #[tokio::test]
    async fn a_sign_out_before_the_move_deletes_both_items() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = Arc::new(MemoryWallet::with_v1("RT-OLD"));
        let (config, id, secrets) = migrated(dir.path(), &wallet).await;
        wallet.store(&Slot::Account(id.clone()), "x", "RT-NEW").await.unwrap();

        secrets.delete().await.unwrap();

        assert_eq!((wallet.current(&Slot::V1), wallet.current(&Slot::Account(id.clone()))), (None, None));
        assert!(!secrets.exists().await.unwrap());
        assert!(!config.account(&id).unwrap().legacy_token);
    }

    /// Uses the real Secret Service (KWallet) with test-only attributes.
    /// Run explicitly: `cargo test -p konedrived secret_service_round_trip -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn secret_service_round_trip() {
        let wallet = SecretServiceWallet::for_tests();
        let slot = Slot::Account("0123456789ab".into());
        wallet.store(&slot, "KOneDrive test", "test-value").await.unwrap();
        assert!(wallet.exists(&slot).await.unwrap());
        assert!(!wallet.exists(&Slot::V1).await.unwrap(), "an account's item is not version 1's");
        assert_eq!(wallet.load(&slot).await.unwrap().as_deref(), Some("test-value"));
        wallet.delete(&slot).await.unwrap();
        assert!(!wallet.exists(&slot).await.unwrap());
    }
}
