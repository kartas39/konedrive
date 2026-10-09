//! Whether programs may change the folder: a read-only folder refuses every change and shows
//! no write bit; a read-write one takes them, and its outbox sends them.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::Errno;

use super::access_mode::AccessMode;
use super::mount::Mount;
use super::shared::Shared;

impl Shared {
    pub(super) fn writable(&self) -> bool {
        self.writable.load(Ordering::SeqCst)
    }

    /// The access mode of the folder now.
    pub(super) fn access_mode(&self) -> AccessMode {
        if self.writable() {
            AccessMode::ReadWrite
        } else {
            AccessMode::ReadOnly
        }
    }

    /// Refuses a change to a read-only folder as a read-only filesystem does.
    pub(super) fn may_change(&self) -> Result<(), Errno> {
        if self.writable() {
            Ok(())
        } else {
            Err(Errno::EROFS)
        }
    }
}

impl Mount {
    /// The folder takes changes from now on, and `wake` is called for each one recorded:
    /// its sync uploads. Whether the modes shown changed with that.
    pub fn read_write(&self, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.shared.outbox_recorder.set_wake(Some(wake));
        let changed = !self.shared.writable.swap(true, Ordering::SeqCst);
        if changed {
            // What a lost row left unsent is found again.
            let s = Arc::clone(&self.shared);
            self.shared.outbox_recorder.run(move || s.write_operations.record_unrecorded_entries());
        }
        changed
    }

    /// The folder takes no change from now on. Whether the modes shown changed with that.
    pub fn read_only(&self) -> bool {
        self.shared.outbox_recorder.set_wake(None);
        self.shared.writable.swap(false, Ordering::SeqCst)
    }

    pub fn is_writable(&self) -> bool {
        self.shared.writable()
    }
}
