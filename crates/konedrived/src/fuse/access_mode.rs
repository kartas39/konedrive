//! Whether programs may change the folder.

/// The access mode of a folder: a read-only one refuses every change and shows no write bit;
/// a read-write one takes changes, and its outbox sends them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccessMode {
    ReadOnly,
    ReadWrite,
}
