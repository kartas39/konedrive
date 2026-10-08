//! Whether programs may change the folder: a read-only folder refuses every change and shows
//! no write bit; a read-write one takes them, and its outbox sends them.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::Errno;

use super::mount::Mount;
use super::shared::Shared;

impl Shared {
    pub(super) fn writable(&self) -> bool {
        self.writable.load(Ordering::SeqCst)
    }

    /// The modes the mount shows: of a directory, or of a file.
    pub(super) fn perm(&self, dir: bool) -> u16 {
        match (self.writable(), dir) {
            (true, true) => 0o755,
            (true, false) => 0o644,
            (false, true) => 0o555,
            (false, false) => 0o444,
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

    /// Wakes the outbox's worker: a row was written.
    pub(super) fn wake(&self) {
        let wake = crate::panic::lock(&self.wake).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

impl Mount {
    /// The folder takes changes from now on, and `wake` is called for each one recorded:
    /// its sync uploads. Whether the modes shown changed with that.
    pub fn read_write(&self, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
        *crate::panic::lock(&self.shared.wake) = Some(wake);
        !self.shared.writable.swap(true, Ordering::SeqCst)
    }

    /// The folder takes no change from now on. Whether the modes shown changed with that.
    pub fn read_only(&self) -> bool {
        *crate::panic::lock(&self.shared.wake) = None;
        self.shared.writable.swap(false, Ordering::SeqCst)
    }

    pub fn is_writable(&self) -> bool {
        self.shared.writable()
    }
}
