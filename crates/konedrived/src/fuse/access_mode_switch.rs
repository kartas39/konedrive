//! Whether programs may change the folder.
//!
//! [`AccessModeSwitch`] is the one owner of that fact: it holds the flag, and whoever needs
//! the mode asks it and is answered an [`AccessMode`]. Nothing else keeps the mode, or works
//! it out from something else.
//!
//! Who asks:
//!
//! - a write operation, at its start, on the write-operation thread
//!   ([`AccessModeSwitch::check_read_write`]): the outbox recorder does not ask again;
//! - the request that asks for a write operation, before it puts it on that thread: a
//!   read-only folder's is refused at once, and not when the thread gets to it;
//! - the kernel's requests that no write operation answers: the attributes shown, `access`,
//!   and the changes the mount never takes ([`AccessModeSwitch::access_mode`]);
//! - the step of a cycle of the delta listing, and the mount, for the folder's sync.
//!
//! It is switched by the mount alone, when the folder's sync starts or stops uploading, and
//! on the write-operation thread ([`AccessModeSwitch::switch_on_thread`]): no write
//! operation runs while the mode changes, so the mode an operation was answered at its
//! start holds until it has recorded, and nothing is recorded in a folder that was switched
//! to read-only.

use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::Arc;

use fuser::Errno;
use tokio::sync::oneshot;

use super::write_operation_thread::WriteOperationThread;

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

    /// Has the switch to `access_mode` made on `write_operation_thread`, after every job the
    /// thread was given before; whether it changed the mode comes through what this
    /// returns, once it is made.
    pub(super) fn queue_switch(self: &Arc<Self>, write_operation_thread: &WriteOperationThread, access_mode: AccessMode) -> oneshot::Receiver<bool> {
        let switch = Arc::clone(self);
        write_operation_thread.run_and_wait(move || switch.switch_to(access_mode))
    }

    /// Switches to `access_mode` on `write_operation_thread`, and waits for it: every write
    /// operation that was asked for before the switch has ended when this returns, and each
    /// one after it finds the new mode. Whether that changed the mode. A thread that is
    /// stopped runs no write operation any more, and the switch is made here.
    pub(super) async fn switch_on_thread(self: &Arc<Self>, write_operation_thread: &WriteOperationThread, access_mode: AccessMode) -> bool {
        match self.queue_switch(write_operation_thread, access_mode).await {
            Ok(changed) => changed,
            Err(_) => self.switch_to(access_mode),
        }
    }

    /// Switches to `access_mode`. Whether that changed the mode.
    fn switch_to(&self, access_mode: AccessMode) -> bool {
        match access_mode {
            AccessMode::ReadWrite => self.switch_to_read_write(),
            AccessMode::ReadOnly => self.switch_to_read_only(),
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
