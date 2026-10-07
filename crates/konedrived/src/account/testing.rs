//! Test support for the account, also for the tests of the daemon, of `konedrivectl` and the
//! VM suite: a wallet in memory, and one account with no accounts manager around it. Built
//! only under the crate's own tests and the `testing` feature; no build that ships has it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use konedrive_graph::oauth::Endpoints;

use crate::account::secret::{SecretError, SecretStore, Slot, Wallet, V1_LABEL};
use crate::account::AccountService;
use crate::config::{ConfigStore, Paths, MIGRATED_LABEL};

/// One account in a configuration of its own under `dir` (`config.toml`, and the account's
/// files in `accounts/<id>/`): the configuration's first account, or a new one called
/// `Personal`. It signs in with the client id `config.toml` has, or konedrive's own.
pub async fn single_account(
    dir: &Path,
    endpoints: Endpoints,
    secrets: Arc<dyn SecretStore>,
    sign_in_timeout: Duration,
) -> anyhow::Result<Arc<AccountService>> {
    let paths = Paths::in_dir(dir);
    let config = Arc::new(ConfigStore::open(&paths, async { false }).await);
    let id = match config.snapshot().accounts.first() {
        Some(account) => account.id.clone(),
        None => config.add_account(MIGRATED_LABEL)?.id,
    };
    let account_paths = paths.account(&id).ok_or_else(|| anyhow::anyhow!("{id:?} is not an account id"))?;
    AccountService::new(config, &id, account_paths, endpoints, secrets, sign_in_timeout)
}

/// A wallet in memory: each slot's label and secret.
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

    /// How many items the wallet holds.
    pub fn count(&self) -> usize {
        self.items.lock().unwrap().len()
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
