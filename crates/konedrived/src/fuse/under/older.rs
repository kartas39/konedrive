//! A folder an older konedrive kept: it holds placeholders, files that have an item's name and
//! none of its content. A mount is never put over one: what it holds would be taken for
//! downloaded copies or for files of the user's own.

use std::fs;
use std::io;
use std::path::Path;

use konedrive_fs::placeholder::{State, XATTR_STATE};

use crate::folder::disk::HOLDING;

/// Whether `folder`, which is not mounted over yet, holds what an older konedrive left: a
/// file marked as anything but whole, or that konedrive's holding directory.
pub(in crate::fuse) fn left_by_an_older(folder: &Path) -> bool {
    let mut dirs = vec![folder.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let (path, Ok(kind)) = (entry.path(), entry.file_type()) else { continue };
            if entry.file_name() == HOLDING {
                return true;
            }
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() && xattr::get(&path, XATTR_STATE).ok().flatten().is_some_and(|state| state != State::Hydrated.as_str().as_bytes()) {
                return true;
            }
        }
    }
    false
}

/// Why no mount is put over such a folder.
pub(in crate::fuse) fn refusal(folder: &Path) -> io::Error {
    io::Error::other(format!("{} holds the files of an older konedrive; forget the folder and add it again", folder.display()))
}

#[cfg(test)]
mod tests;
