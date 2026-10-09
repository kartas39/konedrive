//! The reads that wait for a range from OneDrive, by the number the kernel gave the request:
//! a program that is interrupted or killed has its read given up (FUSE's `interrupt`), and
//! so has every read when the mount is taken off.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::task::AbortHandle;

/// The number the kernel gives a request, and names it by when the program behind it is
/// interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(in crate::fuse) struct RequestId(pub(in crate::fuse) u64);

/// The reads in flight, each by its request id.
#[derive(Default)]
pub(in crate::fuse) struct InterruptibleReads(Mutex<HashMap<RequestId, AbortHandle>>);

impl InterruptibleReads {
    /// The read of the request `request_id` is in flight, and is given up through `read`.
    pub(super) fn add(&self, request_id: RequestId, read: AbortHandle) {
        crate::panic::lock(&self.0).insert(request_id, read);
    }

    /// The read of the request `request_id` ended.
    pub(super) fn remove(&self, request_id: RequestId) {
        crate::panic::lock(&self.0).remove(&request_id);
    }

    /// The program behind the request `request_id` was interrupted: its read is given up, and
    /// answered with an error as its reply is dropped. A request that reads nothing is not
    /// here, and nothing is done.
    pub(super) fn interrupt(&self, request_id: RequestId) {
        if let Some(read) = crate::panic::lock(&self.0).remove(&request_id) {
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
