//! The directories of the folder down to a place: what a downloaded copy, or anything a
//! program makes, stands in. Each is marked with the id of the item it is the directory of,
//! so that it is known as that item's whatever its name becomes.
//!
//! Which item a name is, the disk does not know: whoever asks for the directories says, name
//! by name.

use std::io;
use std::path::{Path, PathBuf};

use super::BackingDirectory;

impl BackingDirectory {
    /// The directories down to `rel`, made where they are missing, each carrying the id of
    /// the item it is the directory of. `root` is the item of the folder itself; `item`
    /// tells the item of a name from the directory it is in — that directory's item, when
    /// it has one, and its place. A directory `item` knows nothing of is made and left
    /// unmarked.
    pub(in crate::fuse) fn backing(&self, rel: &Path, root: Option<String>, item: impl Fn(Option<&str>, &Path, &str) -> Option<String>) -> io::Result<()> {
        let (mut above, mut dir) = (root, PathBuf::new());
        for name in rel.components() {
            let at = dir.join(name);
            let id = name.as_os_str().to_str().and_then(|n| item(above.as_deref(), &dir, n));
            match self.make_dir(&at) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            if let Some(id) = &id {
                if self.carried(&at).as_deref() != Some(id.as_str()) {
                    self.carry(&at, id)?;
                }
            }
            (above, dir) = (id, at);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
