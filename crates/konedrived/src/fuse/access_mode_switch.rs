//! Whether programs may change the folder.
//!
//! [`AccessModeSwitch`] is the one owner of that fact: it holds the flag, and whoever needs
//! the mode asks it and is answered an [`AccessMode`]. Nothing else keeps the mode, or works
//! it out from something else.
//!
//! Who asks, each once for what it does:
//!
//! - a write operation, at its start ([`AccessModeSwitch::check_read_write`]): the request
//!   that asked for it does not ask again, and neither does the outbox recorder;
//! - the kernel's requests that no write operation answers: the attributes shown, `access`,
//!   and the changes the mount never takes ([`AccessModeSwitch::access_mode`]);
//! - the mount, for the step of a cycle of the delta listing and for the folder's sync.
//!
//! It is switched by the mount alone, when the folder's sync starts or stops uploading.

use std::sync::atomic::{AtomicBool, Ordering};

use fuser::Errno;

/// The access mode of a folder: a read-only one refuses every change and shows no write bit;
/// a read-write one takes changes, and its outbox sends them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccessMode {
    ReadOnly,
    ReadWrite,
}

/// The holder of a folder's access mode.
pub(super) struct AccessModeSwitch {
    is_read_write: AtomicBool,
}

impl AccessModeSwitch {
    /// The switch of a folder in `access_mode`. A mount starts every folder read-only.
    pub(super) fn new(access_mode: AccessMode) -> Self {
        AccessModeSwitch { is_read_write: AtomicBool::new(access_mode == AccessMode::ReadWrite) }
    }

    /// The folder's access mode now.
    pub(super) fn access_mode(&self) -> AccessMode {
        match self.is_read_write.load(Ordering::SeqCst) {
            true => AccessMode::ReadWrite,
            false => AccessMode::ReadOnly,
        }
    }

    /// Refuses a change to a read-only folder as a read-only filesystem does.
    pub(super) fn check_read_write(&self) -> Result<(), Errno> {
        match self.access_mode() {
            AccessMode::ReadWrite => Ok(()),
            AccessMode::ReadOnly => Err(Errno::EROFS),
        }
    }

    /// The folder takes changes from now on. Whether that changed the mode.
    pub(super) fn switch_to_read_write(&self) -> bool {
        !self.is_read_write.swap(true, Ordering::SeqCst)
    }

    /// The folder takes no change from now on. Whether that changed the mode.
    pub(super) fn switch_to_read_only(&self) -> bool {
        self.is_read_write.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests;
