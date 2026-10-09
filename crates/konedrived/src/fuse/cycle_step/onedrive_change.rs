//! One file or directory of OneDrive a cycle of the delta listing changed: what happened to
//! its name in the folder.

use std::path::PathBuf;

use konedrive_tree::{Kind, Row};

/// One file or directory of OneDrive a cycle changed, by what happened to its name in the
/// folder. Each case carries what exists in it: the row and the path the store had, the row
/// and the path the new tree has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneDriveChange {
    /// It is new in the folder: the new tree has it at `now`.
    Appeared { id: String, new: Row, now: PathBuf },
    /// It is gone from the folder: the store had it at `was`.
    Vanished { id: String, old: Row, was: PathBuf },
    /// It was renamed or moved: the store had it at `was`, the new tree has it at `now`.
    Moved { id: String, old: Row, was: PathBuf, new: Row, now: PathBuf },
    /// It is at `at` as before: something else of it changed.
    Stayed { id: String, old: Row, new: Row, at: PathBuf },
}

impl OneDriveChange {
    /// The change of the OneDrive id `id`, from where the store had it placed (`old`) and
    /// where the new tree has it placed (`new`). `None` for an id that is placed in neither:
    /// nothing of it is in the folder, before or after.
    pub(in crate::fuse) fn of(id: String, old: Option<(Row, PathBuf)>, new: Option<(Row, PathBuf)>) -> Option<Self> {
        match (old, new) {
            (None, None) => None,
            (None, Some((new, now))) => Some(OneDriveChange::Appeared { id, new, now }),
            (Some((old, was)), None) => Some(OneDriveChange::Vanished { id, old, was }),
            (Some((old, was)), Some((new, now))) if was != now => Some(OneDriveChange::Moved { id, old, was, new, now }),
            (Some((old, _)), Some((new, at))) => Some(OneDriveChange::Stayed { id, old, new, at }),
        }
    }

    /// Its OneDrive id.
    pub fn id(&self) -> &str {
        match self {
            OneDriveChange::Appeared { id, .. } | OneDriveChange::Vanished { id, .. } | OneDriveChange::Moved { id, .. } | OneDriveChange::Stayed { id, .. } => id,
        }
    }

    /// Whether it is a file that was in the folder and is still, and whose content is
    /// another now.
    pub fn has_new_content(&self) -> bool {
        match self {
            OneDriveChange::Moved { old, new, .. } | OneDriveChange::Stayed { old, new, .. } => {
                new.kind == Kind::File && (old.ctag != new.ctag || old.quickxor != new.quickxor || old.size != new.size)
            }
            OneDriveChange::Appeared { .. } | OneDriveChange::Vanished { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests;
