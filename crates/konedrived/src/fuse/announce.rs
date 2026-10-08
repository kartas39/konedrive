//! Changes told to the kernel through the mount, so that it tells whoever watches a directory.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};

use fuser::INodeNo;
use konedrive_tree::Kind;

use super::shared::Shared;

pub(super) enum Announce {
    /// A name that came: its place, its kind, and the item it lies in.
    Came(PathBuf, Kind, Option<String>),
    /// A name that went: its place, its kind, its item, and the item it lay in.
    Gone(PathBuf, Kind, String, Option<String>),
    Changed(PathBuf, String),
}

/// What the announcer is telling the kernel right now. The kernel looks a name up before it
/// passes on a create or a remove of it, and the store has the change already: so for this one
/// name the announcer is told what was true before, a name that came as absent and one that went
/// as still there.
pub(super) enum Told {
    Came { parent: u64, name: String },
    Gone { parent: u64, name: String, ino: u64, kind: Kind },
}

impl Shared {
    pub(super) fn announce(&self, what: Vec<Announce>) {
        if what.is_empty() {
            return;
        }
        if let Some(tell) = &*crate::panic::lock(&self.tell) {
            let _ = tell.send(what);
        }
    }
}

/// The announcer's thread: each change is made by path through the mount, which answers it from
/// the store ([`Told`]), so that the kernel tells whoever watches the directory.
pub(super) fn announce(shared: &Arc<Shared>, notifier: &fuser::Notifier, told: mpsc::Receiver<Vec<Announce>>) {
    shared.announcer.store(unsafe { libc::gettid() }, Ordering::SeqCst);
    let path = |rel: &Path| CString::new(shared.root.path.join(rel).into_os_string().into_encoded_bytes()).unwrap_or_default();
    let name = |rel: &Path| rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tell = |told: Told| *crate::panic::lock(&shared.told) = Some(told);
    // The node of an item the kernel was told: the root is always known.
    let node = |id: &Option<String>| -> Option<u64> {
        let id = id.as_deref()?;
        if shared.read(|s| s.root_item_id()).flatten().as_deref() == Some(id) {
            return Some(1);
        }
        crate::panic::lock(&shared.nodes).inos.get(id).copied()
    };
    for a in told.into_iter().flatten() {
        // The mount is going: nothing more is said through it.
        if shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        // SAFETY: plain system calls on a path this function owns.
        unsafe {
            match a {
                Announce::Came(rel, kind, above) => {
                    // A directory the kernel was never told has nobody watching it.
                    let Some(parent) = node(&above) else { continue };
                    tell(Told::Came { parent, name: name(&rel) });
                    match kind {
                        Kind::Folder => libc::mkdir(path(&rel).as_ptr(), 0o755),
                        Kind::File => libc::mknod(path(&rel).as_ptr(), libc::S_IFREG | 0o644, 0),
                    };
                }
                Announce::Gone(rel, kind, id, above) => {
                    // A name the kernel was never told has nobody watching it go.
                    let (Some(parent), Some(ino)) = (node(&above), crate::panic::lock(&shared.nodes).inos.get(&id).copied()) else { continue };
                    tell(Told::Gone { parent, name: name(&rel), ino, kind });
                    match kind {
                        Kind::Folder => libc::rmdir(path(&rel).as_ptr()),
                        Kind::File => libc::unlink(path(&rel).as_ptr()),
                    };
                }
                Announce::Changed(rel, id) => {
                    // The kernel's cached pages of the old content go first.
                    let ino = crate::panic::lock(&shared.nodes).inos.get(&id).copied();
                    if let Some(ino) = ino {
                        let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                    }
                    libc::utimensat(libc::AT_FDCWD, path(&rel).as_ptr(), std::ptr::null(), 0);
                }
            }
        }
        *crate::panic::lock(&shared.told) = None;
    }
}
