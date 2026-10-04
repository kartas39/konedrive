//! A stop of the daemon between two system calls, which nothing but a
//! switch can make happen. Per folder, by its root directory: the reconcile
//! of a test runs on a blocking thread, not on the test's own.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;

/// The root directories (device, inode) whose removals stop.
static STOP_AFTER_UNLINK: Mutex<Option<HashSet<(u64, u64)>>> = Mutex::new(None);

fn key(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// From now on (`stop`), or no longer, a removal in the folder at `root`
/// stops right after its unlink.
pub(in crate::remote::materialize) fn stop_after_unlink(root: &Path, stop: bool) {
    let root = key(&std::fs::metadata(root).expect("the folder"));
    let mut stops = STOP_AFTER_UNLINK.lock().unwrap();
    let stops = stops.get_or_insert_with(HashSet::new);
    if stop {
        stops.insert(root);
    } else {
        stops.remove(&root);
    }
}

pub(super) fn stops_after_unlink(root: &std::fs::File) -> bool {
    let Ok(meta) = root.metadata() else { return false };
    STOP_AFTER_UNLINK.lock().unwrap().as_ref().is_some_and(|stops| stops.contains(&key(&meta)))
}

/// What a test does in the folder at a root between the look at what can no
/// longer be placed and its removal, once: what a user does in that moment.
type Meanwhile = Box<dyn FnOnce() + Send>;
static BEFORE_REMOVAL: Mutex<Vec<((u64, u64), Meanwhile)>> = Mutex::new(Vec::new());

/// `meanwhile` runs once, right before the next removal of what can no
/// longer be placed in the folder at `root`.
pub(in crate::remote) fn before_the_next_removal(root: &Path, meanwhile: impl FnOnce() + Send + 'static) {
    let root = key(&std::fs::metadata(root).expect("the folder"));
    BEFORE_REMOVAL.lock().unwrap().push((root, Box::new(meanwhile)));
}

pub(super) fn before_removal(root: &std::fs::File) {
    let Ok(meta) = root.metadata() else { return };
    let waiting = {
        let mut all = BEFORE_REMOVAL.lock().unwrap();
        all.iter().position(|(at, _)| *at == key(&meta)).map(|n| all.remove(n).1)
    };
    if let Some(meanwhile) = waiting {
        meanwhile();
    }
}
