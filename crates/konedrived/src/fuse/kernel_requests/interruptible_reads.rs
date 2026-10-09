//! The reads that wait for a range from OneDrive, by the number the kernel gave the request:
//! a program that is interrupted or killed has its read given up (FUSE's `interrupt`), and
//! so has every read when the mount is taken off.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::task::AbortHandle;

/// The reads in flight, each by its request number.
#[derive(Default)]
pub(in crate::fuse) struct InterruptibleReads(Mutex<HashMap<u64, AbortHandle>>);

impl InterruptibleReads {
    /// The read of the request `request` is in flight, and is given up through `read`.
    pub(super) fn add(&self, request: u64, read: AbortHandle) {
        crate::panic::lock(&self.0).insert(request, read);
    }

    /// The read of the request `request` ended.
    pub(super) fn remove(&self, request: u64) {
        crate::panic::lock(&self.0).remove(&request);
    }

    /// The program behind the request `request` was interrupted: its read is given up, and
    /// answered with an error as its reply is dropped. A request that reads nothing is not
    /// here, and nothing is done.
    pub(super) fn interrupt(&self, request: u64) {
        if let Some(read) = crate::panic::lock(&self.0).remove(&request) {
            read.abort();
        }
    }

    /// Every read in flight is given up: the mount is being taken off.
    pub(in crate::fuse) fn abort_all(&self) {
        crate::panic::lock(&self.0).drain().for_each(|(_, read)| read.abort());
    }
}

#[cfg(test)]
mod tests;
