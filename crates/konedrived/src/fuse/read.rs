//! A file a program holds open.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

/// A file a program holds open: its node id, its OneDrive id when it is in OneDrive, its path
/// in the folder, the file in the backing directory once there is one, and whether the
/// program may write it. A write operation that opens a file returns one, and the request
/// that asked keeps it in the table of open files.
pub(super) struct OpenFile {
    pub(super) ino: u64,
    pub(super) id: Option<String>,
    pub(super) rel: PathBuf,
    pub(super) copy: Option<Arc<File>>,
    pub(super) writes: bool,
}
