//! Local filesystem primitives for a folder's files: konedrive's extended
//! attributes and modes, write leases, file handles and the feature probe.

pub mod handle;
pub mod lease;
pub mod placeholder;
pub mod probe;

/// How deep below a sync root any walk of the tree goes before it gives up on
/// that branch, the root itself being depth 0.
///
/// A walk holds one directory descriptor per level of the branch it is on, so
/// this is also what bounds its descriptor use, and what stops a deliberately
/// pathological tree from recursing without end.
pub const MAX_DEPTH: usize = 128;

/// Linux's limit on one name, in bytes (OneDrive's is 255
/// characters, and a Cyrillic character is two bytes).
pub const NAME_MAX: usize = 255;

/// The prefix of the daemon's own working names in a folder.
pub const RESERVED_PREFIX: &str = ".konedrive-";

/// The path of an open descriptor, for the few APIs that still take one.
///
/// Using `/proc/self/fd/<n>` rather than a path string means that whatever is
/// done through it lands on the exact object the descriptor was opened on,
/// and cannot be redirected by swapping a component of a path afterwards.
/// The path is good only while the descriptor is open.
pub fn proc_path(fd: &impl std::os::fd::AsFd) -> std::path::PathBuf {
    use std::os::fd::AsRawFd;
    std::path::PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}
