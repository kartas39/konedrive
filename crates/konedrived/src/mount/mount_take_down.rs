//! The mount's way to say it must be taken down and the folder started over.
//!
//! **State owned:** whether a take-down was asked for (asked once, the first cause
//! stands). Set by the write operations and the steps towards the folder through
//! [`MountTakeDown::ask`], which itself switches the folder to read-only, so that no
//! further change is taken, and reports to the owner of the folder's state through the
//! report it was made with.
//!
//! **Answers for:** design §8: "a row that cannot be written is undone on disk; if the
//! undo fails too, the mount is taken down and the daemon starts over through the start-up
//! check". The owner of the folder's state (`SyncService`, inside a change of the folder)
//! stops the tasks, stops the mount, closes the store and runs the start again
//! (`sync_tasks::stop_order::take_folder_down`, `start_order::start_folder_sync`).
//!
//! **Tested without the rest:**
// test: a_take_down_asked_for_switches_the_folder_to_read_only_and_reports_once
// test: a_second_cause_does_not_report_again
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;
use std::sync::Arc;

use super::access_mode_switch::AccessModeSwitch;

/// Why the mount cannot go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeDownCause {
    /// A change of the backing directory whose row could not be written could not be
    /// undone either: the disk has at `path` what no row says.
    UndoFailed { path: PathBuf },
    /// A backing file could not be made or cut after the row that says so was written:
    /// the row says of the file at `path` what the disk does not hold.
    BackingFileBehindRow { path: PathBuf },
    /// The store failed under a write operation in a way that leaves the disk ahead of the
    /// rows.
    StoreFailed { detail: String },
}

/// Who is told of a take-down: the owner of the folder's state. Called once, from the
/// write-operation thread: it must not block (it sends to the owner's task).
pub type TakeDownReport = Arc<dyn Fn(TakeDownCause) + Send + Sync>;

/// The request of one mount.
pub struct MountTakeDown {
    access_mode_switch: Arc<AccessModeSwitch>,
    report: TakeDownReport,
    is_asked: std::sync::atomic::AtomicBool,
}

impl MountTakeDown {
    /// Nothing asked. Made by the mount's build, with the mount's switch and the report
    /// of the mount's setup.
    pub(super) fn new(access_mode_switch: Arc<AccessModeSwitch>, report: TakeDownReport) -> Self {
        MountTakeDown { access_mode_switch, report, is_asked: std::sync::atomic::AtomicBool::new(false) }
    }

    /// Asks for the take-down: the folder is read-only from this line on, and the owner is
    /// told, once. Called on the write-operation thread, which answers the program `EIO`.
    pub(super) fn ask(&self, cause: TakeDownCause) {
        // Read-only first, whoever asked first: no further change is taken from here on.
        self.access_mode_switch.switch_to_read_only();
        if !self.is_asked.swap(true, std::sync::atomic::Ordering::SeqCst) {
            (self.report)(cause);
        }
    }

    /// Whether a take-down was asked for. Called by the steps towards the folder, which
    /// then end as `StepObstacle::SendingStopped` without touching the disk.
    pub fn is_asked(&self) -> bool {
        self.is_asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests;
