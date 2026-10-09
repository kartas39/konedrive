//! Whether programs may change the folder.
//!
//! [`AccessMode`] owns the one flag that says so. It is read by whoever shows or takes a
//! change — the attributes, the kernel's requests, the write operations and the outbox
//! recorder — and switched by the mount alone, when the folder's sync starts or stops
//! uploading.

use std::sync::atomic::{AtomicBool, Ordering};

use fuser::Errno;

/// The access mode of a folder: a read-only one refuses every change and shows no write bit;
/// a read-write one takes changes, and its outbox sends them.
pub(super) struct AccessMode {
    read_write: AtomicBool,
}

impl AccessMode {
    /// The access mode of a folder that takes no change yet: every folder starts read-only.
    pub(super) fn read_only() -> Self {
        AccessMode { read_write: AtomicBool::new(false) }
    }

    /// The access mode of a folder that takes changes.
    #[cfg(test)]
    pub(super) fn read_write() -> Self {
        AccessMode { read_write: AtomicBool::new(true) }
    }

    /// Whether the folder takes changes now.
    pub(super) fn is_read_write(&self) -> bool {
        self.read_write.load(Ordering::SeqCst)
    }

    /// Refuses a change to a read-only folder as a read-only filesystem does.
    pub(super) fn check_read_write(&self) -> Result<(), Errno> {
        if self.is_read_write() {
            Ok(())
        } else {
            Err(Errno::EROFS)
        }
    }

    /// The folder takes changes from now on. Whether that changed the mode.
    pub(super) fn switch_to_read_write(&self) -> bool {
        !self.read_write.swap(true, Ordering::SeqCst)
    }

    /// The folder takes no change from now on. Whether that changed the mode.
    pub(super) fn switch_to_read_only(&self) -> bool {
        self.read_write.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests;
