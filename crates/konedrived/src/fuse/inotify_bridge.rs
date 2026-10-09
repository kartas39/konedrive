//! A change from OneDrive brought to whoever watches the folder: inotify tells of what is
//! done through the mount, and of nothing the daemon changes in the store behind it.
//!
//! [`InotifyBridge`] owns a thread that makes each change by path through the mount — a name
//! that appeared is made, one that vanished is removed, a file whose content changed is
//! touched — so that the kernel tells whoever watches the directory. The mount recognises
//! the thread's requests ([`InotifyBridge::is_own_thread`]) and answers them without making
//! anything: the store has the change already. The bridge owns the thread, the way to it,
//! the thread's id, the announcement in progress, and its own stop.
//!
//! The kernel looks a name up before it passes on a create or a remove of it, and the store
//! has the change already: so for the one name being announced the bridge's thread is
//! answered what was true before the change ([`find_name_before_change`]) — a name that
//! appeared as absent, and one that vanished as still there.
//!
//! The node ids come from the [`NodeResolver`]: a directory the kernel was never told of has
//! nobody watching it, and nothing is announced in it.

use std::ffi::{CString, OsStr};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use fuser::INodeNo;
use konedrive_tree::Kind;

use super::directory_contents::DirectoryContents;
use super::node_resolver::NodeResolver;

/// A change from OneDrive, or of what the mount shows of a OneDrive file, to announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteChange {
    /// A name appeared at `rel`, in the directory with the OneDrive id `parent_id`.
    NameAppeared { rel: PathBuf, kind: Kind, parent_id: Option<String> },
    /// The name of the OneDrive id `id` vanished from `rel`, in the directory with the
    /// OneDrive id `parent_id`.
    NameVanished { rel: PathBuf, kind: Kind, id: String, parent_id: Option<String> },
    /// What the mount shows of the OneDrive file `id` at `rel` changed: its content, or how
    /// much of it is on this computer.
    ContentChanged { rel: PathBuf, id: String },
}

/// The announcement the bridge's thread is making right now: the name, in the directory node
/// `parent`, that the kernel looks up before it passes the change on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AnnouncementInProgress {
    NameAppearing { parent: u64, name: String },
    /// The name of the node `node_id` is being removed.
    NameVanishing { parent: u64, name: String, node_id: u64, kind: Kind },
}

/// What a name was before the change that is being announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NameBeforeChange {
    /// It appeared: there was no such name.
    Absent,
    /// It vanished: the node `node_id` had it.
    Present { node_id: u64, kind: Kind },
}

/// What `name` in the directory node `parent` was before the change, when it is the name
/// `in_progress` announces: the answer the bridge's thread gets to its lookup. `None` for
/// every other name, which is answered as the folder has it now.
pub(super) fn find_name_before_change(in_progress: Option<&AnnouncementInProgress>, parent: u64, name: &OsStr) -> Option<NameBeforeChange> {
    match in_progress? {
        AnnouncementInProgress::NameAppearing { parent: in_, name: appearing } if *in_ == parent && name == appearing.as_str() => Some(NameBeforeChange::Absent),
        AnnouncementInProgress::NameVanishing { parent: in_, name: vanishing, node_id, kind } if *in_ == parent && name == vanishing.as_str() => {
            Some(NameBeforeChange::Present { node_id: *node_id, kind: *kind })
        }
        _ => None,
    }
}

pub(super) struct InotifyBridge {
    /// The folder's path, which is the mount: where the changes are made.
    folder: PathBuf,
    node_resolver: Arc<NodeResolver>,
    directory_contents: Arc<DirectoryContents>,
    /// The way to the thread; `None` once the bridge is stopped.
    changes: Mutex<Option<mpsc::Sender<Vec<RemoteChange>>>>,
    /// What the thread reads from, until it is started.
    queued: Mutex<Option<mpsc::Receiver<Vec<RemoteChange>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// The thread's id, as the kernel names it in a request; 0 before the thread runs.
    thread_id: AtomicI32,
    in_progress: Mutex<Option<AnnouncementInProgress>>,
    /// The mount is being taken off: nothing more is announced through it.
    stopping: AtomicBool,
}

impl InotifyBridge {
    /// The bridge of the folder mounted at `folder`. What it is given to announce waits
    /// until its thread is started ([`start`](Self::start)), once the mount answers.
    pub(super) fn new(folder: PathBuf, node_resolver: Arc<NodeResolver>, directory_contents: Arc<DirectoryContents>) -> Self {
        let (changes, queued) = mpsc::channel();
        InotifyBridge {
            folder,
            node_resolver,
            directory_contents,
            changes: Mutex::new(Some(changes)),
            queued: Mutex::new(Some(queued)),
            thread: Mutex::new(None),
            thread_id: AtomicI32::new(0),
            in_progress: Mutex::new(None),
            stopping: AtomicBool::new(false),
        }
    }

    /// Starts the thread, which announces through the mount `notifier` is of. Called once,
    /// when the mount answers requests.
    pub(super) fn start(self: &Arc<Self>, notifier: fuser::Notifier) -> io::Result<()> {
        let Some(queued) = crate::panic::lock(&self.queued).take() else { return Ok(()) };
        let bridge = Arc::clone(self);
        let thread = std::thread::Builder::new().name("konedrive-announce".into()).spawn(move || bridge.announce_queued(&notifier, queued))?;
        *crate::panic::lock(&self.thread) = Some(thread);
        Ok(())
    }

    /// Has `changes` announced, in their order, after what was given before. A bridge that
    /// is stopped drops them.
    pub(super) fn announce(&self, changes: Vec<RemoteChange>) {
        if changes.is_empty() {
            return;
        }
        if let Some(way) = &*crate::panic::lock(&self.changes) {
            let _ = way.send(changes);
        }
    }

    /// Whether a request of the thread `thread_id` is the bridge's own: a change it
    /// announces, which the mount answers from the store and never makes.
    pub(super) fn is_own_thread(&self, thread_id: u32) -> bool {
        thread_id as i32 == self.thread_id.load(Ordering::SeqCst)
    }

    /// What `name` in the directory node `parent` was before the change being announced
    /// right now ([`find_name_before_change`]).
    pub(super) fn find_name_before_change(&self, parent: u64, name: &OsStr) -> Option<NameBeforeChange> {
        find_name_before_change(crate::panic::lock(&self.in_progress).as_ref(), parent, name)
    }

    /// Stops the bridge and waits for its thread: what is being announced is finished, what
    /// is queued is abandoned, and what comes later is dropped. Called while the mount
    /// still answers: what the thread does by path would otherwise land in the backing
    /// directory.
    pub(super) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        *crate::panic::lock(&self.changes) = None;
        if let Some(thread) = crate::panic::lock(&self.thread).take() {
            let _ = thread.join();
        }
    }

    /// The node id of the directory with the OneDrive id `id`, when the kernel was told of
    /// it: the root is always known.
    fn find_directory_node_id(&self, id: Option<&str>) -> Option<u64> {
        let id = id?;
        if self.directory_contents.find_root_id().as_deref() == Some(id) {
            return Some(1);
        }
        self.node_resolver.find_node_id(id)
    }

    /// The thread: makes each change by path through the mount, which answers it from the
    /// store, so that the kernel tells whoever watches the directory.
    fn announce_queued(&self, notifier: &fuser::Notifier, queued: mpsc::Receiver<Vec<RemoteChange>>) {
        // SAFETY: a system call that takes nothing and cannot fail.
        self.thread_id.store(unsafe { libc::gettid() }, Ordering::SeqCst);
        let path = |rel: &Path| CString::new(self.folder.join(rel).into_os_string().into_encoded_bytes()).unwrap_or_default();
        let name = |rel: &Path| rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let begin = |announcement: AnnouncementInProgress| *crate::panic::lock(&self.in_progress) = Some(announcement);
        for change in queued.into_iter().flatten() {
            // The mount is going: nothing more is announced through it.
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            // SAFETY: plain system calls on a path this function owns.
            unsafe {
                match change {
                    RemoteChange::NameAppeared { rel, kind, parent_id } => {
                        // A directory the kernel was never told of has nobody watching it.
                        let Some(parent) = self.find_directory_node_id(parent_id.as_deref()) else { continue };
                        begin(AnnouncementInProgress::NameAppearing { parent, name: name(&rel) });
                        match kind {
                            Kind::Folder => libc::mkdir(path(&rel).as_ptr(), 0o755),
                            Kind::File => libc::mknod(path(&rel).as_ptr(), libc::S_IFREG | 0o644, 0),
                        };
                    }
                    RemoteChange::NameVanished { rel, kind, id, parent_id } => {
                        // A name the kernel was never told of has nobody watching it go.
                        let (Some(parent), Some(node_id)) = (self.find_directory_node_id(parent_id.as_deref()), self.node_resolver.find_node_id(&id)) else { continue };
                        begin(AnnouncementInProgress::NameVanishing { parent, name: name(&rel), node_id, kind });
                        match kind {
                            Kind::Folder => libc::rmdir(path(&rel).as_ptr()),
                            Kind::File => libc::unlink(path(&rel).as_ptr()),
                        };
                    }
                    RemoteChange::ContentChanged { rel, id } => {
                        // The kernel's cached pages of the old content go first.
                        if let Some(node_id) = self.node_resolver.find_node_id(&id) {
                            let _ = notifier.inval_inode(INodeNo(node_id), 0, 0);
                        }
                        libc::utimensat(libc::AT_FDCWD, path(&rel).as_ptr(), std::ptr::null(), 0);
                    }
                }
            }
            *crate::panic::lock(&self.in_progress) = None;
        }
    }
}

#[cfg(test)]
mod tests;
