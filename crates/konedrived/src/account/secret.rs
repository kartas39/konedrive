//! Refresh-token storage: Secret Service (KWallet on Plasma). The tests' wallet, in memory, is
//! `crate::account::testing::MemoryWallet`.
//!
//! Each account keeps its token in an item of its own ([`Slot::Account`]); version 1's one
//! item ([`Slot::V1`]) is moved into the migrated account's the first time its token is
//! loaded ([`AccountSecrets`], `docs/design/accounts.md` §8.4).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::config::{AccountId, ConfigError, ConfigStore};

pub use konedrive_graph::secret::{SecretError, SecretStore};

/// Where a refresh token is kept in the wallet.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Version 1's one item: `application=konedrive, kind=refresh-token`.
    V1,
    /// An account's own: `application=konedrive, kind=account-refresh-token, account=<id>`.
    /// A `kind` of its own, because a search matches every item whose attributes *include*
    /// the ones asked for: a search for version 1's item would otherwise find these too.
    Account(AccountId),
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

/// The label version 1 gave its item, and an account's before its email is known.
#[cfg_attr(not(any(test, feature = "testing")), allow(dead_code))]
pub(super) const V1_LABEL: &str = "KOneDrive refresh token";

/// One account's refresh token (§8.4): its own item, and — while the account's
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
    id: AccountId,
    /// The email the token belongs to, for the item's label; empty until known.
    email: Mutex<String>,
}

impl AccountSecrets {
    pub fn new(wallet: Arc<dyn Wallet>, config: Arc<ConfigStore>, id: &AccountId) -> Self {
        Self { wallet, config, id: id.clone(), email: Mutex::new(String::new()) }
    }

    fn slot(&self) -> Slot {
        Slot::Account(self.id.clone())
    }

    fn label(&self) -> String {
        let email = crate::panic::lock(&self.email);
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
        *crate::panic::lock(&self.email) = email.to_owned();
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
                attributes.insert("account", id.to_string());
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
mod tests;
