use std::time::Duration;
use std::sync::Arc;

use super::*;

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..600 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

// --- InodeLocks (Ruling: serialization) -----------

/// A file's `(dev, ino)`, the way every caller of `InodeLocks` gets one.
pub(crate) fn key_of(path: &std::path::Path) -> InodeKey {
    InodeKey::of(&std::fs::File::open(path).unwrap()).unwrap()
}

/// The property a path key cannot have: two names for one
/// inode are one key, and two different files are two keys.
#[test]
fn a_key_names_the_inode_and_not_the_name_it_was_reached_by() {
    let dir = tempfile::tempdir().unwrap();
    let one = dir.path().join("one.bin");
    let another = dir.path().join("another.bin");
    std::fs::write(&one, b"x").unwrap();
    std::fs::write(&another, b"x").unwrap();
    let link = dir.path().join("link.bin");
    std::fs::hard_link(&one, &link).unwrap();
    let renamed = dir.path().join("renamed.bin");

    assert_eq!(key_of(&one), key_of(&link), "a hard link is the same inode");
    assert_ne!(key_of(&one), key_of(&another), "two files are two inodes");
    let before = key_of(&one);
    std::fs::rename(&one, &renamed).unwrap();
    assert_eq!(before, key_of(&renamed), "a rename changes no inode");
}

/// The core property: a second waiter on the *same* key does not run
/// until the first holder's guard drops. Measured by ordering, not by
/// timing alone — `order` only ever gets `"b"` pushed onto it after
/// `"a-still-holding"`, which can only happen if `locks.lock` really
/// blocked task B for the whole time A held its guard.
#[tokio::test]
async fn the_second_waiter_on_the_same_key_does_not_run_until_the_first_releases() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"x").unwrap();
    let key = key_of(&dir.path().join("f"));
    let order: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let guard_a = locks.lock(key).await;

    let order_b = Arc::clone(&order);
    let locks_b = locks.clone();
    let key_b = key;
    let waiter = tokio::spawn(async move {
        let _guard_b = locks_b.lock(key_b).await;
        order_b.lock().unwrap().push("b");
    });

    // Give the waiter every chance to (wrongly) run before A releases.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    order.lock().unwrap().push("a-still-holding");

    drop(guard_a);
    waiter.await.unwrap();

    assert_eq!(
        *order.lock().unwrap(),
        vec!["a-still-holding", "b"],
        "the second waiter must not enter until the first guard is dropped"
    );
}

/// The other half: this is a per-key lock, not a single global one — an
/// unrelated file must never wait on this one's holder.
#[tokio::test]
async fn a_different_key_is_not_blocked_by_an_unrelated_one() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"x").unwrap();
    std::fs::write(dir.path().join("b"), b"x").unwrap();
    let _held = locks.lock(key_of(&dir.path().join("a"))).await;

    let other = tokio::time::timeout(
        Duration::from_millis(200),
        locks.lock(key_of(&dir.path().join("b"))),
    )
    .await;
    assert!(other.is_ok(), "an unrelated key must not block on this one's holder");
}

/// The table must not grow without bound: once the only guard for a key
/// is dropped, that key's row is gone, not merely unlocked.
#[tokio::test]
async fn a_releasing_key_is_removed_from_the_table() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let guard = locks.lock(key_of(&dir.path().join("x"))).await;
    assert_eq!(locks.tracked(), 1, "the key must be tracked while held");
    drop(guard);
    assert_eq!(
        locks.tracked(),
        0,
        "a key with no more holders or waiters must not stay in the table forever"
    );
}

/// The other direction, and the one the previous bookkeeping got wrong:
/// a row must **survive** while somebody else still needs it. Dropping
/// it there would hand the next caller a brand-new mutex for an inode
/// another task is already working on — two fills of one file, which is
/// exactly what this table exists to prevent, arrived at through the
/// cleanup rather than through the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_stays_while_another_caller_is_still_using_it() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let key = key_of(&dir.path().join("x"));

    let held_by_b = Arc::new(tokio::sync::Notify::new());
    let guard_a = locks.lock(key).await;
    let locks_b = locks.clone();
    let told = Arc::clone(&held_by_b);
    let b = tokio::spawn(async move {
        let _guard = locks_b.lock(key).await;
        told.notify_one();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    // Wait until B is genuinely counted as waiting. Spinning on
    // `yield_now` and hoping does not do it: on a multi-threaded runtime
    // B may not have reached `lock` at all when A releases, and then the
    // cleanup this test is about never has two callers to choose
    // between — the over-eager version passes exactly as the correct one
    // does. (Measured: with the yield-only version, the "drop the row
    // while another caller holds it" mutant survived.)
    wait_until("the second caller is waiting", || locks.users(key) == 2).await;
    drop(guard_a);
    held_by_b.notified().await;

    // B holds it now. A third caller must wait for B — which it can only
    // do if A's release left B's row in the table.
    let third = tokio::time::timeout(Duration::from_millis(300), locks.lock(key)).await;
    assert!(
        third.is_err(),
        "a third caller entered while another still held the same inode: the row was \
         dropped from the table while it was in use"
    );
    b.abort();
}

/// before the fix: holder releases, parked waiter is cancelled, one row
/// stays in the table forever.
#[tokio::test]
async fn a_cancelled_waiter_leaves_no_row_behind() {
    let locks = InodeLocks::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let key = key_of(&dir.path().join("x"));

    let guard = locks.lock(key).await;
    let locks_b = locks.clone();
    let waiter = tokio::spawn(async move {
        let _guard = locks_b.lock(key).await;
    });
    wait_until("the waiter is parked", || locks.users(key) == 2).await;
    waiter.abort();
    // Awaiting the handle after `abort` is what guarantees the task's
    // future has actually been dropped, not merely told to stop.
    assert!(waiter.await.unwrap_err().is_cancelled());
    drop(guard);

    assert_eq!(
        locks.tracked(),
        0,
        "a cancelled waiter left its row in the table: the table grows by one row per \
         cancelled call, forever"
    );
}
