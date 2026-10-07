use super::*;
use crate::account::testing::MemoryWallet;
use crate::config::Paths;
use konedrive_graph::secret::MemoryStore;

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
async fn migrated(dir: &std::path::Path, wallet: &Arc<MemoryWallet>) -> (Arc<ConfigStore>, AccountId, AccountSecrets) {
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

/// `docs/design/accounts.md` §8.4 (test 3): version 1's item counts before the move, moves into the
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
    let slot = Slot::Account(AccountId::new("0123456789ab"));
    wallet.store(&slot, "KOneDrive test", "test-value").await.unwrap();
    assert!(wallet.exists(&slot).await.unwrap());
    assert!(!wallet.exists(&Slot::V1).await.unwrap(), "an account's item is not version 1's");
    assert_eq!(wallet.load(&slot).await.unwrap().as_deref(), Some("test-value"));
    wallet.delete(&slot).await.unwrap();
    assert!(!wallet.exists(&slot).await.unwrap());
}
