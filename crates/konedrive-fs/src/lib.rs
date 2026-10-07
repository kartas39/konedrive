//! Local filesystem primitives for placeholders: xattr state, sparse files,
//! hole punching, write leases, file handles and the feature probe.

pub mod handle;
pub mod lease;
pub mod placeholder;
pub mod probe;

/// How deep below a sync root any walk of the tree goes before it gives up on
/// that branch, the root itself being depth 0.
///
/// One constant for both halves of the system, because the two walks only
/// make sense at the same depth. The helper's walk (`konedrive-helper`'s
/// `marks::walk_below`) refuses to mark a directory past it, so nothing
/// below is ever intercepted; the daemon's startup recovery
/// (`konedrived`'s `hydration::recovery::recover`) stops at the same level and counts
/// what it left, because recovering files no open of which could ever be
/// intercepted is incoherent, not merely slow. It lives here, in the crate
/// both depend on, so that the two cannot drift apart: they used to be two
/// literals commented as having to agree.
///
/// Both walks hold one directory descriptor per level of the branch they are
/// on, so this is also what bounds their descriptor use, and what stops a
/// deliberately pathological tree from recursing without end.
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
