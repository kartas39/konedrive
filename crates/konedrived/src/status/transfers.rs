//! The downloads under way, as `Transfers` publishes them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

/// One download under way, as `Transfers.Downloads` publishes it: (full path,
/// bytes done, bytes total); and whether it is a file being opened (or
/// `Hydrate`), which `LargeFiles` leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub path: String,
    pub done: u64,
    pub total: u64,
    pub open: bool,
}

/// `Transfers.LargeFiles`: the large files ([`LARGE_FROM`](konedrive_graph::pool::LARGE_FROM)
/// and up) the sync moves now, each once however many streams it runs — the downloads of that
/// size but the files being opened, and the uploads of that size.
pub fn large_files(downloads: &BTreeMap<u64, Transfer>, uploads: &[(String, u64, u64)]) -> u32 {
    let large = |total: u64| total >= konedrive_graph::pool::LARGE_FROM;
    let down = downloads.values().filter(|t| !t.open && large(t.total)).count();
    let up = uploads.iter().filter(|(_, _, total)| large(*total)).count();
    u32::try_from(down + up).unwrap_or(u32::MAX)
}

/// The downloads under way (`Transfers`): fills on open,
/// `Hydrate`, and replacements of changed files — not thumbnails.
///
/// Watched, so that `dbus::signals` can publish it coalesced. An entry is added
/// by [`start`](Self::start) and removed when the [`TransferEntry`] it
/// returns is dropped, however the download ends.
#[derive(Clone)]
pub struct Transfers {
    tx: Arc<watch::Sender<BTreeMap<u64, Transfer>>>,
    next: Arc<AtomicU64>,
}

impl Default for Transfers {
    fn default() -> Self {
        Self { tx: Arc::new(watch::Sender::new(BTreeMap::new())), next: Arc::new(AtomicU64::new(0)) }
    }
}

impl Transfers {
    /// A download of `path`, `total` bytes as far as is known yet.
    pub fn start(&self, path: String, total: u64) -> TransferEntry {
        self.start_as(path, total, false)
    }

    /// As [`start`](Self::start), for a file being opened (or `Hydrate`) when `open`.
    pub fn start_as(&self, path: String, total: u64, open: bool) -> TransferEntry {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.tx.send_modify(|all| {
            all.insert(id, Transfer { path, done: 0, total, open });
        });
        TransferEntry { handle: TransferHandle { id, transfers: self.clone() } }
    }

    /// Every download under way, oldest first.
    pub fn list(&self) -> Vec<Transfer> {
        self.tx.borrow().values().cloned().collect()
    }

    pub fn subscribe(&self) -> watch::Receiver<BTreeMap<u64, Transfer>> {
        self.tx.subscribe()
    }

    fn progress(&self, id: u64, done: u64, total: u64) {
        self.tx.send_if_modified(|all| match all.get_mut(&id) {
            Some(entry) if (entry.done, entry.total) != (done, total) => {
                entry.done = done;
                entry.total = total;
                true
            }
            _ => false,
        });
    }

    fn remove(&self, id: u64) {
        self.tx.send_if_modified(|all| all.remove(&id).is_some());
    }
}

/// A download's entry in [`Transfers`], removed when this is dropped.
pub struct TransferEntry {
    handle: TransferHandle,
}

impl TransferEntry {
    pub fn progress(&self, done: u64, total: u64) {
        self.handle.progress(done, total);
    }

    /// Something that can move the entry on but does not keep it: the
    /// stream a download reads.
    pub fn handle(&self) -> TransferHandle {
        self.handle.clone()
    }

    /// How big the download is, as far as its source has said.
    pub fn total(&self) -> u64 {
        self.handle.transfers.tx.borrow().get(&self.handle.id).map_or(0, |t| t.total)
    }
}

impl Drop for TransferEntry {
    fn drop(&mut self) {
        self.handle.transfers.remove(self.handle.id);
    }
}

#[derive(Clone)]
pub struct TransferHandle {
    id: u64,
    transfers: Transfers,
}

impl TransferHandle {
    pub fn progress(&self, done: u64, total: u64) {
        self.transfers.progress(self.id, done, total);
    }
}

#[cfg(test)]
mod tests;
