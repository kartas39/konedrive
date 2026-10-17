//! A path as a column of the store keeps it.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rusqlite::types::{Value, ValueRef};

/// A path as the store keeps it: text when it is UTF-8, its bytes otherwise
/// (Linux names need not be UTF-8; such a name is blocked, and still has to
/// be listed where it is). One path always gets the same form, so equality
/// in SQL holds.
pub(crate) fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(text) => Value::Text(text.to_owned()),
        None => Value::Blob(path.as_os_str().as_bytes().to_vec()),
    }
}

pub(crate) fn path_from(value: ValueRef<'_>) -> PathBuf {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => PathBuf::from(OsStr::from_bytes(bytes)),
        _ => PathBuf::new(),
    }
}
