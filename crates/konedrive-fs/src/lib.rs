//! Local filesystem primitives for placeholders: xattr state, sparse files,
//! hole punching, write leases and the feature probe.

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
/// (`konedrived`'s `sync::root::recover`) stops at the same level and counts
/// what it left, because recovering files no open of which could ever be
/// intercepted is incoherent, not merely slow. It lives here, in the crate
/// both depend on, so that the two cannot drift apart: they used to be two
/// literals commented as having to agree.
///
/// Both walks hold one directory descriptor per level of the branch they are
/// on, so this is also what bounds their descriptor use, and what stops a
/// deliberately pathological tree from recursing without end.
pub const MAX_DEPTH: usize = 128;
