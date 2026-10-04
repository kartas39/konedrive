//! Where an object is now, asked of the helper by its handle, and the proof that it is
//! absent from a place.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use konedrive_fs::handle::FileHandle;

use super::{Place, Run};
use crate::local::liveness::Whereabouts;

impl Run<'_, '_, '_> {
    /// Where the object `handle` names is: the liveness answer, placed. An
    /// answer is "outside" only when the path is sure and not beneath the
    /// root; "inside" only when the object's own handle stands at that place
    /// beneath the root. Anything else decides nothing.
    ///
    /// An ask that times out is the last one of the run: every wait is spent
    /// with the tree lock held, and a helper that did not answer one question
    /// in its whole timeout is not asked the next. What is not asked is not
    /// decided, and is looked at again like anything undecided.
    pub(super) fn place_of(&self, handle: &FileHandle) -> Place {
        if self.helper_silent.get() {
            return Place::Unknown;
        }
        let path = match self.ex.liveness.whereabouts(handle) {
            Ok(Whereabouts::Gone) if self.handles_current => return Place::Gone,
            Ok(Whereabouts::Gone) => return Place::Unknown,
            Ok(Whereabouts::At(path)) => path,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                tracing::warn!("the helper did not say where an object is: nothing more is asked in this examination, and what is missing waits");
                self.helper_silent.set(true);
                return Place::Unknown;
            }
            Err(err) => {
                tracing::debug!("an object's whereabouts cannot be asked yet: {err}");
                return Place::Unknown;
            }
        };
        let Some(root) = &self.root_path else { return Place::Unknown };
        let sure = path.is_absolute()
            && !path.as_os_str().as_bytes().ends_with(b" (deleted)")
            && path.components().all(|c| !matches!(c, Component::ParentDir | Component::CurDir));
        if !sure {
            return Place::Unknown;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return Place::Outside(path);
        };
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return Place::Unknown };
        match self.ex.disk.dir(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()) {
            Some(there) if &there == handle => Place::Inside(rel.to_path_buf()),
            _ => Place::Unknown,
        }
    }

    /// Whether the object `handle` names is proved absent from its place `rel`
    /// beneath the root ([`crate::local::liveness::absent_below`]).
    pub(super) fn absent(&self, rel: &Path, handle: &FileHandle) -> bool {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return false };
        crate::local::liveness::absent_below(self.ex.disk.dir(parent), name, handle)
    }

    /// [`absent`](Self::absent) for `at`, inside the folder at `folder` — which
    /// is now at `went_to` if it left the folder.
    pub(super) fn absent_with(&self, at: &Path, folder: &Path, went_to: Option<&Path>, handle: &FileHandle) -> bool {
        match (went_to, at.strip_prefix(folder)) {
            (Some(went), Ok(inside)) => crate::local::liveness::absent_at(&went.join(inside), handle),
            (Some(_), Err(_)) => false,
            (None, _) => self.absent(at, handle),
        }
    }
}
