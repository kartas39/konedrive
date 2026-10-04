//! Test doubles of `local/`, for this area's tests and for the others' that run an examination.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use konedrive_fs::handle::FileHandle;

use super::liveness::{Liveness, Whereabouts};

/// A table of where objects went, for tests: a handle it knows is alive
/// there, any other is gone. Never for the daemon itself: "gone" by default
/// would delete in OneDrive whatever it was not told about — so it is in a
/// module only test builds have.
#[derive(Debug, Default)]
pub struct FakeLiveness {
    alive: Mutex<HashMap<FileHandle, PathBuf>>,
    asked: Mutex<Vec<FileHandle>>,
}

impl FakeLiveness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The object `handle` names now lives at `path`.
    pub fn alive(&self, handle: FileHandle, path: impl Into<PathBuf>) {
        self.alive.lock().unwrap().insert(handle, path.into());
    }

    /// The object at `path` is alive there, and so is everything below it —
    /// as the helper answers for what a moved folder took along.
    pub fn alive_tree(&self, path: &Path) {
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { continue };
            if let Ok(handle) = File::open(parent).and_then(|dir| FileHandle::at(&dir, name)) {
                self.alive(handle, p.clone());
            }
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                stack.extend(std::fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
            }
        }
    }

    /// Every handle asked about, in order.
    pub fn asked(&self) -> Vec<FileHandle> {
        self.asked.lock().unwrap().clone()
    }
}

impl Liveness for FakeLiveness {
    fn whereabouts(&self, handle: &FileHandle) -> io::Result<Whereabouts> {
        self.asked.lock().unwrap().push(handle.clone());
        Ok(match self.alive.lock().unwrap().get(handle) {
            Some(path) => Whereabouts::At(path.clone()),
            None => Whereabouts::Gone,
        })
    }
}
