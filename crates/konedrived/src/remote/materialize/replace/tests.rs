use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::time::Duration;

use async_trait::async_trait;
use konedrive_fs::placeholder::read_ctag;
use konedrive_fs::placeholder::{write_state, State};

use super::*;
use crate::remote::materialize::tests::*;
use crate::remote::materialize::*;
use konedrive_graph::quickxor::QuickXor;
use crate::hydration::source::{ContentSource, Fetched, SourceError, Version};
/// Round 2: `(dev, ino)` alone is not a strong enough identity for a file
/// that was dropped and reopened later — an inode can be freed and
/// reused by an unrelated file in between. `FileIdentity` must tell that
/// case apart, which an ino-only comparison cannot.
#[test]
fn a_reused_inode_with_a_different_birth_time_is_a_different_file() {
    let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500);
    let a = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t1) };
    let same = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t1) };
    let reused = FileIdentity { dev: 1, ino: 7, fingerprint: Fingerprint::Born(t2) };
    assert_eq!(a, same, "the same dev, ino and birth time is the same file");
    assert_ne!(a, reused, "the same ino with a different birth time is a different file");
}

/// Serves `content` as version `ctag`, with its hash; `on_fetch` runs first.
/// `damaged` flips one byte of what it streams, not of what it hashes.
struct Memory {
    ctag: String,
    content: Vec<u8>,
    damaged: bool,
    on_fetch: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Memory {
    fn new(ctag: &str, content: &[u8]) -> Self {
        Self { ctag: ctag.into(), content: content.to_vec(), damaged: false, on_fetch: std::sync::Mutex::new(None) }
    }
}

#[async_trait]
impl ContentSource for Memory {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        if let Some(hook) = self.on_fetch.lock().unwrap().take() {
            hook();
        }
        let mut hash = QuickXor::new();
        hash.update(&self.content);
        let mut served = self.content.clone();
        if self.damaged {
            served[0] ^= 1;
        }
        let start = (from as usize).min(served.len());
        Ok(Fetched {
            served_from: from,
            size: self.content.len() as u64,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_500),
            version: Some(Version { ctag: self.ctag.clone(), quick_xor: Some(hash.finish()) }),
            stream: Box::new(std::io::Cursor::new(served[start..].to_vec())),
        })
    }
}

#[tokio::test]
async fn a_replacement_swaps_in_the_new_version_and_a_reader_keeps_the_old() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, true).unwrap(), f.path("docs/f.txt"));
    f.listed_async(true).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let reader = File::open(&path).unwrap();
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &Memory::new("c2", b"the new version"), &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Replaced), "{outcome:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"the new version");
    let file = File::open(&path).unwrap();
    assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c2"));
    assert!(stamp_matches(&file).unwrap());
    assert_eq!(mode(&path), 0o444);
    let mut old = vec![0u8; 11];
    reader.read_exact_at(&mut old, 0).unwrap();
    assert_eq!(&old, b"old version", "a reader of the old file keeps it");
}

#[tokio::test]
async fn a_replacement_that_does_not_match_its_hash_leaves_the_old_version() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let damaged = Memory { damaged: true, ..Memory::new("c2", b"the new version") };
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &damaged, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Failed(_)), "{outcome:?}");
    assert_eq!((ino(&path), std::fs::read(&path).unwrap()), (before, b"old version".to_vec()));
}

#[tokio::test]
async fn a_replacement_that_cannot_be_downloaded_leaves_the_old_version() {
    struct Gone;
    #[async_trait]
    impl ContentSource for Gone {
        async fn fetch(&self, _: &str, _: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
            Err(SourceError::NotFound("gone".into()))
        }
    }
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &Gone, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Failed(_)), "{outcome:?}");
    assert_eq!((ino(&path), std::fs::read(&path).unwrap()), (before, b"old version".to_vec()));
}

#[tokio::test]
async fn a_file_freed_up_while_its_replacement_downloaded_is_left_as_it_is() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let source = Memory::new("c2", b"the new version");
    let freed = path.clone();
    *source.on_fetch.lock().unwrap() = Some(Box::new(move || {
        let file = File::options().read(true).write(true).open(&freed).unwrap();
        // `old` must not still be open here, or Free up
        // space could not take this file's write lease while its
        // replacement downloads.
        assert!(
            konedrive_fs::lease::WriteLease::take(&file).unwrap().is_some(),
            "the old file's write lease is free while its replacement downloads"
        );
        write_state(&file, State::OnlineOnly).unwrap();
    }));
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &source, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Current), "{outcome:?}");
    assert_eq!(ino(&path), before, "the user freed it up; it is not filled behind their back");
}

/// A folder above the file moves (or is removed) while
/// its replacement downloads. `disk.dir(parent)` then answers ENOENT —
/// the same "nothing to do any more, the next cycle looks again" case as
/// any other change underneath the replacement, not a download failure to
/// report and keep retrying forever.
#[tokio::test]
async fn a_folder_moved_while_its_replacement_downloaded_is_left_as_it_is() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let before = ino(&path);
    let source = Memory::new("c2", b"the new version");
    let root = f.root.path.clone();
    *source.on_fetch.lock().unwrap() = Some(Box::new(move || {
        std::fs::rename(root.join("docs"), root.join("papers")).unwrap();
    }));
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let outcome = replace(&disk, &InodeLocks::new(), &source, &replacement).await;
    assert!(matches!(outcome, ReplaceOutcome::Current), "{outcome:?}");
    assert_eq!(ino(&f.path("papers/f.txt")), before, "the file is untouched at its new path");
    assert_eq!(std::fs::read(f.path("papers/f.txt")).unwrap(), b"old version");
}

/// The swap under the old file's lock really does
/// wait for it — untested until now — rather than racing whoever holds
/// it (a fill, a Free up, another replacement of the same file).
#[tokio::test]
async fn a_replacements_swap_waits_for_the_per_inode_lock() {
    let f = fixture_async();
    let (disk, path) = (Disk::open(&f.root, false).unwrap(), f.path("docs/f.txt"));
    f.listed_async(false).await;
    hydrate_by_hand(&path, b"old version", "c-F");
    let locks = InodeLocks::new();
    let key = crate::folder::locks::InodeKey::of(&File::open(&path).unwrap()).unwrap();
    let held = locks.lock(key).await;

    let task_locks = locks.clone();
    let replacement = Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: "c2".into(), size: 15 };
    let handle = tokio::spawn(async move {
        let source = Memory::new("c2", b"the new version");
        replace(&disk, &task_locks, &source, &replacement).await
    });

    // The download itself is instant (an in-memory source, no delay);
    // this is time enough for the task to reach the lock and block on it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!handle.is_finished(), "the swap has not gone ahead while the lock is held");
    assert_eq!(std::fs::read(&path).unwrap(), b"old version", "not swapped in yet");

    drop(held);
    let outcome = handle.await.unwrap();
    assert!(matches!(outcome, ReplaceOutcome::Replaced), "{outcome:?}");
    assert_eq!(std::fs::read(&path).unwrap(), b"the new version", "swapped in once the lock is free");
}
