//! Moves out of the folder (`docs/design/writes.md` §8, §10, WR5): a `move-out` row's step, the
//! re-marking of what left, and the routing of its fills.
//!
//! **The order is WR5's.** An object that left the folder is reached by its file handle, through
//! the helper (`OpenByHandle`), never by a path the row remembers. Then:
//!
//! - **anywhere but the Trash**, a placeholder is marked again (`MarkFile`, so any open is
//!   intercepted, M4) and downloaded through its own descriptor, the ordinary fill; a directory is
//!   walked and every placeholder of its item downloaded. Only when every byte is local and the
//!   state says so (`hydrated`, the fill's commit point, verified against OneDrive's hash), and the
//!   object is still proved to be outside the folder, is the row marked [`CONTENT_LOCAL`], the
//!   attributes taken off (the item id first), the directories unmarked (`UnmarkDir`, never one
//!   beneath a registered folder), and the item deleted in OneDrive;
//! - **in the Trash** — the user's own (`$XDG_DATA_HOME/Trash`) or a mount's (`.Trash-<uid>`,
//!   `.Trash/<uid>` at the mount's top), holding the entry's `.trashinfo` — nothing is downloaded,
//!   as Windows does: downloaded content stays there as the user's own file, a placeholder is
//!   removed with its `.trashinfo` (proved gone: no link left), and the item goes to OneDrive's
//!   recycle bin. Anything that only looks like a Trash is "anywhere else";
//! - **back inside the folder**, nothing is decided: the examination takes it on, and a row it
//!   records behind this one supersedes it.
//!
//! **Doubt keeps the row.** `EPERM` is never "gone" (a nested subvolume, another owner, an object
//! without the attribute), and neither is a helper that does not answer, a download that stopped
//! part-way, or a place that cannot be proved. `ESTALE` is "gone" only when the handles the store
//! recorded belong to the filesystem the folder is on now ([`handles_current`]), and for the row's
//! own object only when it says so twice, some seconds apart. The row's own markers are the one
//! exception to "`EPERM` is never gone": once [`CONTENT_LOCAL`] or [`TRASHED`] is written, the
//! content was proved local (or the object was in the Trash), and the `EPERM` that follows our own
//! stripping of the attributes is the expected answer.
//!
//! **A crash at each step converges** (§5): before a marker, the row starts again from the object
//! (a fill resumes from its checkpoint); after it, the attributes are taken off what is still
//! reachable (what was stripped already answers `EPERM`, which the marker explains) and the item
//! deleted; a delete whose answer was lost finds `404`.
//!
//! **Descriptors from `OpenByHandle`** are used for what the object is (`fstat`, its attributes),
//! where it is (`/proc/self/fd`, proved by opening that path again), `MarkFile`/`MarkDir`/
//! `UnmarkDir`, and a file's own reopen for writing. A directory's is never an anchor for anything
//! below it (SECURITY.md, F90): the walk opens the directory again by its path, through the user's
//! own lookups, checks that it is the same inode, and goes down one name at a time from there,
//! never following a symlink.
//!
//! [`handles_current`]: crate::sync::local::liveness::handles_current

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID};
use nix::fcntl::{openat2, AtFlags, OFlag, OpenHow, ResolveFlag};
use xattr::FileExt;

use super::engine::{Engine, Fail, Outcome};
use super::{reason, Fault};
use crate::sync::disk::Disk;
use crate::sync::helper::{reopen_for_writing, Clearance, HelperError};
use crate::sync::listing::LinkCell;
use crate::sync::local::liveness::{absent_at, handles_current, same_place};
use crate::sync::local::RECHECK;
use crate::sync::root::SyncRoot;
use crate::sync::source::{self, Answered, ContentSource, FillError};
use crate::sync::{InodeKey, InodeLocks, SyncService};
use crate::tree::outbox::{OutboxKind, OutboxRow};
use crate::tree::{Kind, Placement, Store, Table, TreeError, TreeStore};

/// A `move-out` row's marker, kept in its `snapshot`: the content was proved local, so what
/// follows — the attributes taken off, the item deleted in OneDrive — may run. Written before the
/// first of those, so that a replay after a crash, which finds the attributes gone (`EPERM`),
/// knows that it took them off itself. A row that carries it needs no re-marking.
pub const CONTENT_LOCAL: &str = "moved-out:local";
/// The same for a placeholder in the Trash, removed without a download: the row may delete once
/// it is gone. Such a row is still re-marked while its placeholder is there.
pub const TRASHED: &str = "moved-out:trash";

/// How long a first `ESTALE` for a row's own object waits before a second one is believed.
const GONE_AGAIN: Duration = Duration::from_secs(5);

/// What a move out asks of the helper (`docs/design/writes.md` §8).
#[async_trait]
pub trait Helper: Send + Sync {
    /// `OpenByHandle` ([`crate::sync::helper::HelperLink::open_by_handle`]).
    async fn open_by_handle(&self, dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError>;
    async fn mark_file(&self, file: &File) -> Result<(), HelperError>;
    async fn mark_dir(&self, dir: &File) -> Result<(), HelperError>;
    async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError>;
    /// How a file that may carry an ignore mark is cleared before a fill that could fail and
    /// empty it (`source::hydrate_with`); `None` while there is no link.
    fn clearance(&self) -> Option<Clearance>;
}

/// The helper, over the account's link cell: `NotRunning` while there is no link.
pub struct Linked(pub LinkCell);

impl Linked {
    fn link(&self) -> Result<crate::sync::helper::HelperLink, HelperError> {
        self.0.lock().unwrap().clone().ok_or(HelperError::NotRunning)
    }
}

#[async_trait]
impl Helper for Linked {
    async fn open_by_handle(&self, dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError> {
        self.link()?.open_by_handle(dir, handle).await
    }

    async fn mark_file(&self, file: &File) -> Result<(), HelperError> {
        self.link()?.mark_file(file).await
    }

    async fn mark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.link()?.mark_dir(dir).await
    }

    async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.link()?.unmark_dir(dir).await
    }

    fn clearance(&self) -> Option<Clearance> {
        self.link().ok().map(Clearance::Link)
    }
}

/// Downloads a placeholder in place, through a writable descriptor: the ordinary fill.
#[async_trait]
pub trait Filler: Send + Sync {
    /// `shown` is where the file is now, for `Transfers` and the activity log.
    async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError>;
}

/// A fill from `source`, recorded nowhere (tests and the VM suite).
pub struct SourceFill(pub Arc<dyn ContentSource>);

#[async_trait]
impl Filler for SourceFill {
    async fn fill(&self, file: File, _shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
        source::hydrate_with(file.into(), &*self.0, clearance).await
    }
}

/// The folders registered in this daemon, every account's: nothing beneath one of them is ever
/// unmarked, and an object beneath this account's own is the examination's.
pub type Roots = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

/// What the worker needs for `move-out` rows. Without it they wait, as before the move-out step.
#[derive(Clone)]
pub struct MoveOuts {
    pub helper: Arc<dyn Helper>,
    pub filler: Arc<dyn Filler>,
    /// Told the item ids whose fills belong to this account wherever the objects are now (write
    /// design §4.6, §8.5): each moved-out object's, and what the base has inside a moved-out folder.
    pub route: Option<Arc<dyn Fn(HashSet<String>) + Send + Sync>>,
    /// The user's own Trash (`$XDG_DATA_HOME/Trash`). A mount's `.Trash-<uid>` and `.Trash/<uid>`
    /// are recognised at the mount's top only.
    pub home_trash: Option<PathBuf>,
    pub roots: Roots,
}

/// The worker's own record of what it protected: the rows re-marked on this helper connection,
/// and the ids last handed to [`MoveOuts::route`].
#[derive(Default)]
pub(super) struct Protection {
    marked: HashSet<i64>,
    routes: Option<HashSet<String>>,
}

impl Protection {
    /// The helper came back: its marks are gone.
    pub(super) fn helper_back(&mut self) {
        self.marked.clear();
    }
}

// ---------------------------------------------------------------------------
// the Trash
// ---------------------------------------------------------------------------

/// An entry of a Trash: the top-level object in `files/` and its `.trashinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    pub top: PathBuf,
    pub info: PathBuf,
    /// `.Trash/<uid>`: the shared `.Trash`, which must be a sticky directory.
    pub shared: Option<PathBuf>,
}

/// The Trash entry `path` is in, by its place (the freedesktop.org specification): the user's
/// own (`home_trash`), or `.Trash-<uid>` or `.Trash/<uid>` directly at the top of a mount
/// (`is_mount_point`). A `.Trash-<uid>` anywhere else is an ordinary directory.
pub fn trash_of(path: &Path, home_trash: Option<&Path>, uid: u32, is_mount_point: &dyn Fn(&Path) -> bool) -> Option<TrashEntry> {
    let parts: Vec<Component<'_>> = path.components().collect();
    let uid = uid.to_string();
    for i in 1..parts.len().saturating_sub(1) {
        if parts[i].as_os_str() != "files" {
            continue;
        }
        let trash: PathBuf = parts[..i].iter().collect();
        let name = trash.file_name().map(OsStr::to_os_string).unwrap_or_default();
        let parent = trash.parent();
        let shared = parent.filter(|p| p.file_name() == Some(OsStr::new(".Trash")));
        let is_trash = home_trash.is_some_and(|home| home == trash)
            || (name == OsString::from(format!(".Trash-{uid}")) && parent.is_some_and(is_mount_point))
            || (name == OsString::from(&uid) && shared.and_then(Path::parent).is_some_and(is_mount_point));
        if !is_trash {
            continue;
        }
        let top = parts[i + 1].as_os_str();
        let mut info = top.to_os_string();
        info.push(".trashinfo");
        let shared = shared.filter(|_| home_trash.is_none_or(|home| home != trash)).map(Path::to_path_buf);
        return Some(TrashEntry { top: trash.join("files").join(top), info: trash.join("info").join(info), shared });
    }
    None
}

/// Whether `entry` is a Trash's as a desktop makes one: its `.trashinfo` is there (a conforming
/// desktop writes it before it moves the object), and a shared `.Trash` is a sticky directory,
/// not a symlink.
fn real_trash(entry: &TrashEntry) -> bool {
    let info = std::fs::symlink_metadata(&entry.info).is_ok_and(|m| m.is_file());
    let shared = entry.shared.as_ref().is_none_or(|dir| {
        std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o1000 != 0)
    });
    info && shared
}

/// Whether `path` is where a filesystem is mounted (`/proc/self/mountinfo`).
fn is_mount_point(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else { return false };
    text.lines().filter_map(|line| line.split(' ').nth(4)).any(|at| Path::new(&unescape(at)) == path)
}

/// A mountinfo field: `\040` and the like are octal escapes.
fn unescape(field: &str) -> OsString {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&field[i + 1..i + 4], 8) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    <OsString as std::os::unix::ffi::OsStringExt>::from_vec(out)
}

/// The user's own Trash: `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`. Read from the
/// environment only; nothing is opened.
pub fn home_trash() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data.join("Trash"))
}

// ---------------------------------------------------------------------------
// where an object is
// ---------------------------------------------------------------------------

fn proc_path(fd: &impl AsRawFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Where the object behind `fd` is, proved: `/proc/self/fd` read, and that path opened again
/// by the user's own lookups names the same inode. A file whose dentry the kernel could not
/// connect reads as `/`, which this refuses.
fn verified_path(fd: &File) -> Option<PathBuf> {
    let path = std::fs::read_link(proc_path(fd)).ok()?;
    same_place(&path, fd).then_some(path)
}

/// Where a moved-out object is.
enum Place {
    /// Beneath this account's folder, proved by its handle there: the examination's.
    Inside,
    Trash(TrashEntry),
    /// Anywhere else, at this proved path — or `None`, a place that cannot be proved (a file's
    /// dentry disconnected after a reboot): downloaded, but not stripped or deleted until proved.
    Elsewhere(Option<PathBuf>),
    /// Beneath this account's folder, but not proved: nothing is decided.
    Unknown,
}

/// The real path of the folder's root.
fn root_path(disk: &Disk) -> Option<PathBuf> {
    std::fs::read_link(proc_path(&disk.dir(Path::new("")).ok()?)).ok()
}

fn place_of(e: &Engine, disk: &Disk, fd: &File, handle: &FileHandle) -> Place {
    let Some(path) = verified_path(fd) else { return Place::Elsewhere(None) };
    if let Some(rel) = root_path(disk).and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf)) {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return Place::Unknown };
        return match disk.dir(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()) {
            Some(there) if &there == handle => Place::Inside,
            _ => Place::Unknown,
        };
    }
    let home = e.moved_out().home_trash.as_deref();
    match trash_of(&path, home, nix::unistd::geteuid().as_raw(), &is_mount_point).filter(real_trash) {
        Some(entry) => Place::Trash(entry),
        None => Place::Elsewhere(Some(path)),
    }
}

/// Every registered folder's real path, this account's included.
fn roots(mo: &MoveOuts, disk: &Disk) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = (mo.roots)().iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
    roots.extend(root_path(disk));
    roots
}

/// Whether `path` is beneath (or is) a registered folder.
fn beneath_a_root(mo: &MoveOuts, disk: &Disk, path: &Path) -> bool {
    roots(mo, disk).iter().any(|root| path.starts_with(root))
}

/// Whether `path` is beneath a registered folder other than this account's: another account's.
fn in_another_folder(mo: &MoveOuts, disk: &Disk, path: &Path) -> bool {
    let own = root_path(disk);
    (mo.roots)()
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .filter(|root| own.as_ref() != Some(root))
        .any(|root| path.starts_with(root))
}

/// The check before a marker is written: the row is still the item's newest (a row the
/// examination recorded behind it — the object came back, and went on — supersedes it, and it
/// goes), and the object is still proved to be outside this account's folder. `Some` is what the
/// row does instead.
fn before_marker(e: &Engine, disk: &Disk, row: &OutboxRow, id: &str, object: &File) -> Result<Option<Outcome>, Fail> {
    if let Some(outcome) = superseded(e, row, id)? {
        return Ok(Some(outcome));
    }
    let Some(path) = verified_path(object) else { return Ok(Some(Outcome::backoff(reason::PLACE_UNKNOWN))) };
    if root_path(disk).is_some_and(|root| path.starts_with(root)) {
        return Ok(Some(Outcome::backoff(reason::BACK_INSIDE)));
    }
    Ok(None)
}

/// A newer row of the same item (the examination's, behind this running one) supersedes it: this
/// one goes, unless it has begun to take attributes off already.
fn superseded(e: &Engine, row: &OutboxRow, id: &str) -> Result<Option<Outcome>, Fail> {
    if marker(row) {
        return Ok(None);
    }
    let newer = e.store().with(|s| s.outbox_for_item(id))?.into_iter().any(|r| r.seq > row.seq);
    if !newer {
        return Ok(None);
    }
    tracing::info!("{} came back before its move out was done: the newer change goes instead", row.rel.display());
    e.store().with(|s| s.outbox_drop(row.seq, None, None, None))?;
    Ok(Some(Outcome::Done))
}

fn marker(row: &OutboxRow) -> bool {
    matches!(row.snapshot.as_deref(), Some(CONTENT_LOCAL | TRASHED))
}

/// Where the row's object was last proved to be (kept in its `target_name`): what an `ESTALE` is
/// checked against.
fn last_place(row: &OutboxRow) -> Option<&Path> {
    row.target_name.as_deref().map(Path::new).filter(|p| p.is_absolute())
}

/// Keeps where `object` is now, proved, as the row's last place. A name that is not UTF-8 is not
/// kept: its `ESTALE` stays unproved.
fn remember_place(e: &Engine, row: &OutboxRow, object: &File) -> Result<(), Fail> {
    let Some(path) = verified_path(object) else { return Ok(()) };
    let Some(text) = path.to_str().filter(|t| row.target_name.as_deref() != Some(*t)) else { return Ok(()) };
    Ok(e.store().with(|s| s.outbox_set_target(row.seq, None, Some(text)))?)
}

// ---------------------------------------------------------------------------
// walking a moved-out folder
// ---------------------------------------------------------------------------

/// The directory at `path`, opened by path — through the user's own lookups, never beneath a
/// descriptor `OpenByHandle` gave (F90) — and only if it is still `object`.
fn reopen_dir(path: &Path, object: &File) -> io::Result<Option<File>> {
    let dir = reopen_parent(path)?;
    let (a, b) = (dir.metadata()?, object.metadata()?);
    Ok(((a.dev(), a.ino()) == (b.dev(), b.ino())).then_some(dir))
}

/// `rel` below `top` — a directory opened by path, never one `OpenByHandle` gave — opened beneath
/// it, never through a symlink: a regular file read-only (`O_NONBLOCK`; the daemon's own open is
/// never intercepted), or a directory.
fn open_below(top: &File, rel: &Path, is_dir: bool) -> io::Result<File> {
    let flags = if is_dir { OFlag::O_RDONLY | OFlag::O_DIRECTORY } else { OFlag::O_RDONLY | OFlag::O_NONBLOCK };
    let how = OpenHow::new()
        .flags(flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    Ok(File::from(openat2(top.as_fd(), rel, how)?))
}

fn item_id_of(file: &File) -> Option<String> {
    file.get_xattr(XATTR_ITEM_ID).ok().flatten().and_then(|v| String::from_utf8(v).ok())
}

/// Takes konedrive's attributes off `file`, the item id first: from then on it is an ordinary
/// file (a state with no id is one), whatever a crash leaves of the rest.
fn strip(file: &File) -> io::Result<()> {
    placeholder::with_owner_write(file, || match file.remove_xattr(XATTR_ITEM_ID) {
        Err(e) if e.raw_os_error() != Some(libc::ENODATA) => Err(e),
        _ => Ok(()),
    })?;
    file.sync_all()?;
    placeholder::strip_konedrive_xattrs(file)?;
    file.sync_all()
}

/// One regular file or directory met below a moved-out folder, by its place below it.
#[derive(Debug, Clone)]
struct Met {
    /// Relative to the folder.
    rel: PathBuf,
    id: Option<String>,
    is_dir: bool,
    /// `(st_dev, st_ino)` when it was listed: what is opened again at `rel` must still be it.
    key: (u64, u64),
}

impl Met {
    /// Its directory, relative to the folder.
    fn dir(&self) -> &Path {
        self.rel.parent().unwrap_or(Path::new(""))
    }
}

/// Every regular file and directory below `top` (a directory opened by path), on its device,
/// parents before children: listed one directory at a time, each opened beneath `top`, never
/// through a symlink, and closed again; attributes read by name, never through a symlink.
/// Nothing below is filled.
fn walk(top: &File) -> io::Result<Vec<Met>> {
    let dev = top.metadata()?.dev();
    let mut out = Vec::new();
    let mut dirs: Vec<(PathBuf, Option<(u64, u64)>)> = vec![(PathBuf::new(), None)];
    while let Some((at_rel, key)) = dirs.pop() {
        let at = if at_rel.as_os_str().is_empty() {
            top.try_clone()?
        } else {
            match open_below(top, &at_rel, true) {
                Ok(dir) => dir,
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => continue,
                Err(e) => return Err(e),
            }
        };
        if let Some(key) = key {
            let meta = at.metadata()?;
            if (meta.dev(), meta.ino()) != key {
                continue;
            }
        }
        let mut names: Vec<OsString> = std::fs::read_dir(proc_path(&at))?.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
        names.sort();
        for name in names {
            let stat = match nix::sys::stat::fstatat(at.as_fd(), name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(nix::errno::Errno::ENOENT) => continue,
                Err(e) => return Err(e.into()),
            };
            let kind = stat.st_mode & libc::S_IFMT;
            if (kind != libc::S_IFREG && kind != libc::S_IFDIR) || stat.st_dev != dev {
                continue;
            }
            let is_dir = kind == libc::S_IFDIR;
            let rel = at_rel.join(&name);
            let key = (stat.st_dev, stat.st_ino);
            // `lgetxattr` on the name: a symlink swapped in meanwhile is not followed.
            let id = xattr::get(proc_path(&at).join(&name), XATTR_ITEM_ID).ok().flatten().and_then(|v| String::from_utf8(v).ok());
            if is_dir {
                dirs.push((rel.clone(), Some(key)));
            }
            out.push(Met { rel, id, is_dir, key });
        }
    }
    Ok(out)
}

/// What the walk met at `m`, opened again beneath `top`, and only if it is still that.
fn open_met(top: &File, m: &Met) -> io::Result<File> {
    let file = open_below(top, &m.rel, m.is_dir)?;
    let meta = file.metadata()?;
    if (meta.dev(), meta.ino()) != m.key {
        return Err(io::Error::other(format!("{} changed while it was looked at", m.rel.display())));
    }
    Ok(file)
}

/// The directory `rel` below `top` (`top` itself for `""`).
fn dir_below(top: &File, rel: &Path) -> io::Result<File> {
    if rel.as_os_str().is_empty() {
        top.try_clone()
    } else {
        open_below(top, rel, true)
    }
}

/// Whether a file's content is local.
enum Local {
    Yes,
    /// Not yet, and why: the row is tried again later.
    No(Outcome),
}

impl Engine {
    fn moved_out(&self) -> &MoveOuts {
        self.cfg.moved_out.as_ref().expect("move-out rows run only with MoveOuts")
    }

    /// Makes the file behind `object` (read-only, as `OpenByHandle` gives it) local where it is:
    /// marked again first, probed for a writer (one from before the mark could be writing
    /// into it), then filled through a descriptor of its own for writing, under the per-inode
    /// lock every fill takes. `Yes` only for a file that reads `hydrated` afterwards — the fill's
    /// commit point, after the whole content and its hash.
    async fn make_local(&self, object: &File, shown: &Path) -> Result<Local, Fail> {
        let mo = self.moved_out();
        if matches!(placeholder::read_state(object), Ok(Some(State::Hydrated))) {
            return Ok(Local::Yes);
        }
        if let Err(e) = mo.helper.mark_file(object).await {
            // Without a helper nothing intercepts anything; the fill below needs none.
            tracing::debug!("{} is not marked again yet: {e}", shown.display());
        }
        match konedrive_fs::lease::open_for_writing(object) {
            Ok(false) => {}
            Ok(true) => return Ok(Local::No(Outcome::later(crate::sync::local::examine::OPEN_FOR_WRITING, RECHECK))),
            // Leases off (`fs.leases-enable=0`) or not supported: nothing can tell a writer,
            // so nothing is filled.
            Err(err) => return Ok(Local::No(Outcome::backoff(format!("{}: {err}", reason::NO_LEASE)))),
        }
        // Reopened before the lock: the reopen is an open like any other, and is let through at
        // once only as this daemon's own (F91). A fill it could wait for takes the same lock.
        let reopened = {
            let object = object.try_clone()?;
            super::steps::blocking(move || {
                let fd: OwnedFd = object.try_clone()?.into();
                placeholder::with_owner_write(&object, || reopen_for_writing(&fd))
            })
            .await
        };
        let _inode = self.cfg.locks.lock(InodeKey::of(object)?).await;
        let state = placeholder::read_state(object).map_err(|e| Fail::Io(io::Error::other(e.to_string())))?;
        let clearance = match state {
            Some(State::Hydrated) => return Ok(Local::Yes),
            Some(State::OnlineOnly) => None,
            // A fill that stopped part-way (a crash) left it: continued, from its checkpoint.
            Some(State::Hydrating) => match mo.helper.clearance() {
                Some(clearance) => Some(clearance),
                None => return Ok(Local::No(Outcome::backoff(reason::NO_HELPER))),
            },
            Some(State::Dehydrating) => return Ok(Local::No(Outcome::later(reason::NOT_LOCAL, RECHECK))),
            // An item id with no state: nothing konedrive can fill, and nothing to be sure of.
            None => return Ok(Local::No(Outcome::backoff(reason::NOT_LOCAL))),
        };
        let writable = match reopened {
            Ok(writable) => writable,
            // Leased (`EAGAIN`, F91), or not writable by its owner: tried again later.
            Err(Fail::Io(err)) => return Ok(Local::No(Outcome::backoff(format!("{}: {err}", reason::NOT_LOCAL)))),
            Err(other) => return Err(other),
        };
        match mo.filler.fill(writable, shown, clearance.as_ref()).await {
            Ok(()) if matches!(placeholder::read_state(object), Ok(Some(State::Hydrated))) => Ok(Local::Yes),
            Ok(()) => Ok(Local::No(Outcome::backoff(reason::NOT_LOCAL))),
            Err(e) => {
                tracing::info!("{} could not be downloaded before its item leaves OneDrive: errno {}", shown.display(), e.errno());
                Ok(Local::No(Outcome::backoff(format!("{}: errno {}", reason::DOWNLOAD, e.errno()))))
            }
        }
    }

    /// Writes a row's marker (or takes it off), before anything is taken off or removed.
    fn set_marker(&self, row: &OutboxRow, marker: Option<&str>) -> Result<(), Fail> {
        Ok(self.store().with(|s| s.outbox_set_snapshot(row.seq, marker))?)
    }

    /// Re-marks what the pending `move-out` rows name, and hands their ids to the router: before
    /// anything else runs and again whenever the worker is woken, whatever the rows' states (held,
    /// paused, offline, waiting for a folder), since a placeholder that left reads zeros while
    /// nothing marks it (Z3). Each row is marked once per helper connection
    /// ([`Protection::helper_back`]); an answer that may change (`EAGAIN`, a helper that does not
    /// answer) leaves it for the next look.
    pub(super) async fn protect(&self, disk: &Disk) {
        let Some(mo) = self.cfg.moved_out.as_ref() else { return };
        let Ok(rows) = self.store().with(|s| s.outbox_rows()) else { return };
        let rows: Vec<OutboxRow> = rows.into_iter().filter(|r| r.kind == OutboxKind::MoveOut).collect();
        let mut ids = HashSet::new();
        for row in &rows {
            let Some(id) = &row.item_id else { continue };
            ids.insert(id.clone());
            if let Ok(inside) = self.store().with(|s| s.descendants(Table::Items, id)) {
                ids.extend(inside);
            }
        }
        if let Some(route) = &mo.route {
            let mut protection = self.protection();
            if protection.routes.as_ref() != Some(&ids) {
                protection.routes = Some(ids.clone());
                drop(protection);
                route(ids);
            }
        }
        let Ok(root) = disk.dir(Path::new("")) else { return };
        for row in rows {
            if self.protection().marked.contains(&row.seq) || row.snapshot.as_deref() == Some(CONTENT_LOCAL) {
                continue;
            }
            let Some(handle) = row.inode.as_ref().and_then(|i| i.handle.clone()) else { continue };
            let object = match mo.helper.open_by_handle(&root, &handle).await {
                Ok(object) => File::from(object),
                // Gone, not the user's to have, or no handle: nothing to mark. The row decides.
                Err(HelperError::Refused(libc::ESTALE | libc::EPERM | libc::EINVAL)) => {
                    self.protection().marked.insert(row.seq);
                    continue;
                }
                // Leased, or anything else: asked again at the next look.
                Err(HelperError::Refused(errno)) => {
                    tracing::debug!("{} is not marked again yet: errno {errno}", row.rel.display());
                    continue;
                }
                Err(e) => {
                    tracing::debug!("moved-out objects are not marked again yet: {e}");
                    return;
                }
            };
            if let Err(e) = remember_place(self, &row, &object) {
                tracing::debug!("where {} is now is not kept: {e:?}", row.rel.display());
            }
            let marked = if object.metadata().is_ok_and(|m| m.is_dir()) {
                self.mark_tree(&object).await
            } else if matches!(placeholder::read_state(&object), Ok(Some(State::Hydrated))) {
                Ok(())
            } else {
                mo.helper.mark_file(&object).await
            };
            match marked {
                Ok(()) => {
                    self.protection().marked.insert(row.seq);
                }
                // Asked again at every look, so quietly.
                Err(e) => tracing::debug!("{} is not marked again yet: {e}", row.rel.display()),
            }
        }
    }

    /// `MarkDir` for a moved-out directory and every directory below it, whatever it holds: the
    /// marks it took along are gone once the helper restarts. The first failure is the answer,
    /// and the row is marked again at the next look.
    async fn mark_tree(&self, object: &File) -> Result<(), HelperError> {
        let mo = self.moved_out();
        mo.helper.mark_dir(object).await?;
        let unreadable = |what: &str| HelperError::Io(format!("a moved-out directory cannot be {what}"));
        let top = verified_path(object).and_then(|path| reopen_dir(&path, object).ok().flatten()).ok_or_else(|| unreadable("found by its path"))?;
        let top2 = top.try_clone().map_err(|e| HelperError::Io(e.to_string()))?;
        let met = super::steps::blocking(move || walk(&top2)).await.map_err(|_| unreadable("walked"))?;
        for m in met.iter().filter(|m| m.is_dir) {
            let dir = open_met(&top, m).map_err(|e| HelperError::Io(e.to_string()))?;
            mo.helper.mark_dir(&dir).await?;
        }
        Ok(())
    }

    async fn unmark(&self, disk: &Disk, dir: &File) {
        unmark(self.moved_out(), disk, dir).await;
    }
}

/// `UnmarkDir`, but never for a directory beneath a registered folder, wherever it went
/// meanwhile. A mark left behind costs a round trip per open, which the helper lets through (the
/// files are ordinary now); it goes with the helper's next start.
async fn unmark(mo: &MoveOuts, disk: &Disk, dir: &File) {
    match verified_path(dir) {
        Some(path) if !beneath_a_root(mo, disk, &path) => {
            if let Err(err) = mo.helper.unmark_dir(dir).await {
                tracing::debug!("a moved-out directory keeps its mark: {err}");
            }
        }
        _ => tracing::info!("a directory that left the folder is in a folder again, or cannot be placed: it stays marked"),
    }
}

/// A `move-out` row's step.
pub(super) async fn run(e: &Arc<Engine>, disk: &Disk, row: OutboxRow) -> Result<Outcome, Fail> {
    let Some(mo) = e.cfg.moved_out.as_ref() else {
        return Ok(Outcome::later(reason::MOVE_OUT, Duration::from_secs(3600)));
    };
    let (Some(id), Some(handle)) = (row.item_id.clone(), row.inode.as_ref().and_then(|i| i.handle.clone())) else {
        return Ok(Outcome::blocked("no-handle"));
    };
    let root = disk.dir(Path::new(""))?;
    let object = match mo.helper.open_by_handle(&root, &handle).await {
        Ok(object) => File::from(object),
        // Every decode failure is `ESTALE`: believed only for handles taken on the filesystem the
        // folder is on now.
        Err(HelperError::Refused(libc::ESTALE)) if !handles_current(e.store(), &root) => {
            return Ok(Outcome::backoff(reason::STALE_HANDLE));
        }
        // What this row removed or stripped itself.
        Err(HelperError::Refused(libc::ESTALE)) if marker(&row) => return finish(e, &row).await,
        // Gone: the user deleted it, wherever it was (§5) — said twice, some seconds apart...
        Err(HelperError::Refused(libc::ESTALE)) if row.reason.as_deref() != Some(reason::GONE_ONCE) => {
            return Ok(Outcome::later(reason::GONE_ONCE, GONE_AGAIN));
        }
        // ...and with its evidence: nothing, or another object, where it was last proved to be. An
        // inode that cannot be read says `ESTALE` every time, and stands there.
        Err(HelperError::Refused(libc::ESTALE)) if !last_place(&row).is_some_and(|p| absent_at(p, &handle)) => {
            return Ok(Outcome::backoff(reason::GONE_UNPROVED));
        }
        // Last proved inside another account's folder: that account may have taken it for none
        // of its own. Nothing is deleted in OneDrive.
        Err(HelperError::Refused(libc::ESTALE)) if last_place(&row).is_some_and(|p| in_another_folder(mo, disk, p)) => {
            return kept(e, &row, &id).await;
        }
        Err(HelperError::Refused(libc::ESTALE)) => return gone(e, disk, &row, &id).await,
        // The attributes this very row took off: its content was proved local first.
        Err(HelperError::Refused(libc::EPERM)) if marker(&row) => return finish(e, &row).await,
        // The same object, where it was last proved to be inside another account's folder, with
        // no item id: that account's examination took it for its own, and uploads it (review
        // m5). Still not "gone": nothing is deleted in OneDrive, and the item comes back here.
        Err(HelperError::Refused(libc::EPERM)) if stands_in_another_folder(mo, disk, &row, &handle) => {
            return kept(e, &row, &id).await;
        }
        // Never "gone" (F90): kept, and asked again now and then.
        Err(HelperError::Refused(libc::EPERM)) => return Ok(Outcome::backoff(reason::UNREACHABLE)),
        Err(HelperError::Refused(libc::EAGAIN)) => return Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)),
        Err(HelperError::Refused(libc::EINVAL)) => return Ok(Outcome::blocked("bad-handle")),
        Err(HelperError::Refused(errno)) => return Ok(Outcome::backoff(format!("{}: errno {errno}", reason::UNREACHABLE))),
        Err(other) => {
            tracing::debug!("{}: {other}", row.rel.display());
            return Ok(Outcome::backoff(reason::NO_HELPER));
        }
    };
    if item_id_of(&object).as_deref() != Some(id.as_str()) {
        // The helper hands over only an object carrying an item id: another one's is no answer.
        return Ok(Outcome::blocked("another-item"));
    }
    remember_place(e, &row, &object)?;
    let is_dir = object.metadata()?.is_dir();
    match place_of(e, disk, &object, &handle) {
        // A marker stays: what it took off may be taken off already.
        Place::Inside => {
            if let Some(outcome) = superseded(e, &row, &id)? {
                return Ok(outcome);
            }
            Ok(Outcome::backoff(reason::BACK_INSIDE))
        }
        Place::Unknown => Ok(Outcome::backoff(reason::PLACE_UNKNOWN)),
        // A hard-linked placeholder is not only in the Trash: it is downloaded, as anywhere else.
        Place::Trash(entry) if is_dir || object.metadata()?.nlink() == 1 => {
            if is_dir {
                trashed_folder(e, disk, &row, &id, object, &entry).await
            } else {
                trashed_file(e, disk, &row, &id, object, &entry).await
            }
        }
        Place::Trash(_) | Place::Elsewhere(_) => {
            let shown = verified_path(&object).unwrap_or_else(|| e.cfg.root.path.join(&row.rel));
            if is_dir {
                elsewhere_folder(e, disk, &row, &id, object, &shown).await
            } else {
                elsewhere_file(e, disk, &row, &id, object, &shown).await
            }
        }
    }
}

/// A file moved anywhere but the Trash: downloaded, stripped, then its item deleted (WR5).
async fn elsewhere_file(e: &Arc<Engine>, disk: &Disk, row: &OutboxRow, id: &str, object: File, shown: &Path) -> Result<Outcome, Fail> {
    if let Local::No(outcome) = e.make_local(&object, shown).await? {
        return Ok(outcome);
    }
    if let Some(outcome) = before_marker(e, disk, row, id, &object)? {
        return Ok(outcome);
    }
    e.set_marker(row, Some(CONTENT_LOCAL))?;
    super::steps::blocking(move || strip(&object)).await?;
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} left the folder: downloaded to {}, and removed from OneDrive", row.rel.display(), shown.display());
    finish(e, row).await
}

/// A folder moved anywhere but the Trash: every placeholder of its item downloaded where it is,
/// the attributes taken off and every directory unmarked, then the folder deleted in OneDrive —
/// as a folder delete is: one unguarded `DELETE` of the folder itself, whatever it holds there by
/// then (F82 (10)).
async fn elsewhere_folder(e: &Arc<Engine>, disk: &Disk, row: &OutboxRow, id: &str, object: File, shown: &Path) -> Result<Outcome, Fail> {
    let Some(top) = reopen_dir(shown, &object)? else { return Ok(Outcome::backoff(reason::PLACE_UNKNOWN)) };
    if let Err(err) = e.moved_out().helper.mark_dir(&object).await {
        tracing::debug!("{} is not marked again yet: {err}", shown.display());
    }
    let inside = inside_of(e, id)?;
    let top2 = top.try_clone()?;
    let met = super::steps::blocking(move || walk(&top2)).await?;
    let mut ours: Vec<Met> = Vec::new();
    // Directories holding another item's placeholder (with a row of its own) stay marked.
    let mut keep_marked: HashSet<PathBuf> = HashSet::new();
    let mut found: HashSet<String> = HashSet::new();
    for m in met.iter().filter(|m| !m.is_dir) {
        let Some(item) = &m.id else { continue };
        let file = open_met(&top, m)?;
        if inside.contains(item) {
            if let Local::No(outcome) = e.make_local(&file, &shown.join(&m.rel)).await? {
                return Ok(outcome);
            }
            found.insert(item.clone());
            ours.push(m.clone());
        } else if !matches!(placeholder::read_state(&file), Ok(Some(State::Hydrated))) {
            keep_marked.insert(m.dir().to_path_buf());
        }
    }
    let extra = match left_since(e, disk, row, id, &inside, &found, Some(shown)).await? {
        Ok(extra) => extra,
        Err(outcome) => return Ok(outcome),
    };
    if let Some(outcome) = before_marker(e, disk, row, id, &object)? {
        return Ok(outcome);
    }
    e.set_marker(row, Some(CONTENT_LOCAL))?;
    for (n, m) in ours.into_iter().enumerate() {
        let top = top.try_clone()?;
        super::steps::blocking(move || strip(&open_met(&top, &m)?)).await?;
        if n == 0 {
            e.fault(Fault::MidStrip)?;
        }
    }
    super::steps::blocking(move || {
        for file in &extra {
            strip(file)?;
        }
        Ok(())
    })
    .await?;
    // Bottom up, the folder itself last.
    let dirs: Vec<Met> = met.iter().rev().filter(|m| m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))).cloned().collect();
    for m in dirs.iter().map(Some).chain(std::iter::once(None)) {
        let dir = match m {
            Some(m) => open_met(&top, m)?,
            None => top.try_clone()?,
        };
        if !keep_marked.contains(m.map_or(Path::new(""), |m| m.rel.as_path())) {
            e.unmark(disk, &dir).await;
        }
        super::steps::blocking(move || strip(&dir)).await?;
    }
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} left the folder: downloaded to {}, and removed from OneDrive", row.rel.display(), shown.display());
    finish(e, row).await
}

/// A file moved to the Trash: nothing is downloaded (Windows does the same). Downloaded content
/// stays there as the user's own file; a placeholder, which holds nothing, is removed with its
/// `.trashinfo`, and only once it has no link left does the item go to OneDrive's recycle bin.
async fn trashed_file(e: &Arc<Engine>, disk: &Disk, row: &OutboxRow, id: &str, object: File, entry: &TrashEntry) -> Result<Outcome, Fail> {
    let Some(path) = verified_path(&object) else { return Ok(Outcome::backoff(reason::PLACE_UNKNOWN)) };
    let key = InodeKey::of(&object)?;
    let Some(_inode) = e.cfg.locks.try_lock(key) else { return Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)) };
    if let Some(outcome) = before_marker(e, disk, row, id, &object)? {
        return Ok(outcome);
    }
    match placeholder::read_state(&object) {
        Ok(Some(State::Hydrated)) => {
            e.set_marker(row, Some(CONTENT_LOCAL))?;
            super::steps::blocking(move || strip(&object)).await?;
        }
        // Holds nothing whole: the cloud keeps it, in its recycle bin.
        Ok(Some(State::OnlineOnly | State::Hydrating)) => {
            e.set_marker(row, Some(TRASHED))?;
            let (entry, removed) = (entry.clone(), object.try_clone()?);
            super::steps::blocking(move || remove(&removed, &path, Some(&entry))).await?;
            // Proved gone: no link left. Renamed meanwhile, or linked elsewhere, it is found where
            // it is at the next run.
            if object.metadata()?.nlink() != 0 {
                e.set_marker(row, None)?;
                return Ok(Outcome::backoff(reason::PLACE_UNKNOWN));
            }
        }
        _ => return Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)),
    }
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} was moved to the Trash: it is in OneDrive's recycle bin", row.rel.display());
    finish(e, row).await
}

/// A folder moved to the Trash: nothing is downloaded into it. Its downloaded files stay, as the
/// user's own; its placeholders go (each proved gone), and so do its directories left empty, and
/// the whole entry with its `.trashinfo` when nothing is left. A placeholder with another link is
/// downloaded instead.
async fn trashed_folder(e: &Arc<Engine>, disk: &Disk, row: &OutboxRow, id: &str, object: File, entry: &TrashEntry) -> Result<Outcome, Fail> {
    let Some(path) = verified_path(&object) else { return Ok(Outcome::backoff(reason::PLACE_UNKNOWN)) };
    let Some(top) = reopen_dir(&path, &object)? else { return Ok(Outcome::backoff(reason::PLACE_UNKNOWN)) };
    let inside = inside_of(e, id)?;
    let top2 = top.try_clone()?;
    let met = super::steps::blocking(move || walk(&top2)).await?;
    let mut found: HashSet<String> = HashSet::new();
    // Held until the placeholders are gone: no fill starts meanwhile.
    let mut guards = Vec::new();
    // Each file of the item, and whether it stays (downloaded) or goes (a placeholder).
    let mut files: Vec<(Met, bool)> = Vec::new();
    for m in met.iter().filter(|m| !m.is_dir) {
        let Some(item) = m.id.as_ref().filter(|i| inside.contains(*i)) else { continue };
        let file = open_met(&top, m)?;
        if file.metadata()?.nlink() > 1 {
            if let Local::No(outcome) = e.make_local(&file, &path.join(&m.rel)).await? {
                return Ok(outcome);
            }
        }
        let Some(guard) = e.cfg.locks.try_lock(InodeKey::of(&file)?) else { return Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)) };
        let stays = match placeholder::read_state(&file) {
            Ok(Some(State::Hydrated)) => true,
            Ok(Some(State::OnlineOnly | State::Hydrating)) => false,
            _ => return Ok(Outcome::later(reason::NOT_LOCAL, RECHECK)),
        };
        guards.push(guard);
        found.insert(item.clone());
        files.push((m.clone(), stays));
    }
    let extra = match left_since(e, disk, row, id, &inside, &found, Some(&path)).await? {
        Ok(extra) => extra,
        Err(outcome) => return Ok(outcome),
    };
    if let Some(outcome) = before_marker(e, disk, row, id, &object)? {
        return Ok(outcome);
    }
    e.set_marker(row, Some(TRASHED))?;
    // The placeholders go first, each proved gone; nothing is stripped until they all are, so
    // that the marker can be taken off again with nothing stripped.
    let removed_all = {
        let (top, files) = (top.try_clone()?, files.clone());
        super::steps::blocking(move || {
            let mut removed_all = true;
            for (m, _) in files.iter().filter(|(_, stays)| !stays) {
                let file = open_met(&top, m)?;
                if let Some(name) = m.rel.file_name() {
                    removed_all &= remove_at(&file, &dir_below(&top, m.dir())?, name)? && file.metadata()?.nlink() == 0;
                }
            }
            Ok(removed_all)
        })
        .await?
    };
    drop(guards);
    if !removed_all {
        // A placeholder renamed or linked meanwhile: nothing goes until it is found again.
        e.set_marker(row, None)?;
        return Ok(Outcome::backoff(reason::PLACE_UNKNOWN));
    }
    {
        let top = top.try_clone()?;
        super::steps::blocking(move || {
            for file in &extra {
                strip(file)?;
            }
            for (m, _) in files.iter().filter(|(_, stays)| *stays) {
                strip(&open_met(&top, m)?)?;
            }
            Ok(())
        })
        .await?;
    }
    // Bottom up: each directory of the item unmarked, stripped, and removed if left empty.
    for m in met.iter().rev().filter(|m| m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))) {
        let dir = open_met(&top, m)?;
        e.unmark(disk, &dir).await;
        let parent = dir_below(&top, m.dir())?;
        let name = m.rel.file_name().map(OsStr::to_os_string);
        super::steps::blocking(move || {
            strip(&dir)?;
            if let Some(name) = name {
                remove_empty_dir(&dir, &parent, &name);
            }
            Ok(())
        })
        .await?;
    }
    e.unmark(disk, &top).await;
    let entry = entry.clone();
    super::steps::blocking(move || {
        strip(&top)?;
        // The whole entry went: its `.trashinfo` goes too.
        if path == entry.top && std::fs::read_dir(proc_path(&top))?.next().is_none() {
            if let (Some(parent), Some(name)) = (path.parent().and_then(|p| reopen_parent(p).ok()), path.file_name()) {
                remove_empty_dir(&top, &parent, name);
                if !parent_has(&parent, name) {
                    remove_info(&entry);
                }
            }
        }
        Ok(())
    })
    .await?;
    e.fault(Fault::AfterStrip)?;
    tracing::info!("{} was moved to the Trash: it is in OneDrive's recycle bin", row.rel.display());
    finish(e, row).await
}

/// The ids the base has inside folder `id` now, the folder's own included.
fn inside_of(e: &Engine, id: &str) -> Result<HashSet<String>, Fail> {
    let mut inside: HashSet<String> = e.store().with(|s| s.descendants(Table::Items, id))?.into_iter().collect();
    inside.insert(id.to_owned());
    Ok(inside)
}

/// What the base still has inside folder `id` that was placed here and is not among what the
/// walk found (`found`): each is asked after by its own handle. Gone is fine (deleted by the
/// user) when the handles are this filesystem's; so is `EPERM` once the row is marked (it took
/// that file's attributes off itself); a file alive outside the folder left the moved-out folder
/// since, and is made local where it is, like the folder's own (returned, to be stripped with
/// them); anything else — alive in the folder, unreachable, unanswered, with no handle — keeps
/// the folder in OneDrive for now.
async fn left_since(
    e: &Arc<Engine>,
    disk: &Disk,
    row: &OutboxRow,
    id: &str,
    inside: &HashSet<String>,
    found: &HashSet<String>,
    top: Option<&Path>,
) -> Result<Result<Vec<File>, Outcome>, Fail> {
    let mo = e.moved_out();
    let root = disk.dir(Path::new(""))?;
    let folder = e.store().with(|s| s.locate(Table::Items, id))?.map(|l| l.rel);
    let mut extra = Vec::new();
    for item in inside.iter().filter(|i| i.as_str() != id && !found.contains(*i)) {
        let Some(base) = e.store().with(|s| s.get(Table::Items, item))? else { continue };
        if base.kind != Kind::File || base.placement != Placement::Placed {
            continue;
        }
        let Some(handle) = e.store().with(|s| s.local_handle(item))? else {
            return Ok(Err(Outcome::backoff(reason::UNREACHABLE)));
        };
        let object = match mo.helper.open_by_handle(&root, &handle).await {
            Ok(object) => File::from(object),
            Err(HelperError::Refused(libc::ESTALE)) if !handles_current(e.store(), &root) => {
                return Ok(Err(Outcome::backoff(reason::STALE_HANDLE)));
            }
            // Gone with its evidence: nothing, or another object, at its place in the folder
            // where the folder is now.
            Err(HelperError::Refused(libc::ESTALE)) => {
                let at = e.store().with(|s| s.locate(Table::Items, item))?.map(|l| l.rel);
                let there = match (top, folder.as_deref(), at.as_deref()) {
                    (Some(top), Some(folder), Some(at)) => at.strip_prefix(folder).ok().map(|inside| top.join(inside)),
                    _ => None,
                };
                if there.is_some_and(|p| absent_at(&p, &handle)) {
                    continue;
                }
                return Ok(Err(Outcome::backoff(reason::GONE_UNPROVED)));
            }
            Err(HelperError::Refused(libc::EPERM)) if marker(row) => continue,
            Err(HelperError::Refused(_)) => return Ok(Err(Outcome::backoff(reason::UNREACHABLE))),
            Err(_) => return Ok(Err(Outcome::backoff(reason::NO_HELPER))),
        };
        let shown = match place_of(e, disk, &object, &handle) {
            Place::Elsewhere(Some(path)) => path,
            Place::Trash(entry) => entry.top,
            Place::Elsewhere(None) => return Ok(Err(Outcome::backoff(reason::PLACE_UNKNOWN))),
            Place::Inside | Place::Unknown => return Ok(Err(Outcome::backoff(reason::BACK_INSIDE))),
        };
        if let Local::No(outcome) = e.make_local(&object, &shown).await? {
            return Ok(Err(outcome));
        }
        extra.push(object);
    }
    Ok(Ok(extra))
}

/// The item leaves OneDrive, as a delete does (§4.7): `If-Match`, `404` done — a folder, unguarded,
/// whole, whatever changed inside it since.
async fn finish(e: &Arc<Engine>, row: &OutboxRow) -> Result<Outcome, Fail> {
    super::steps::delete(e, row.clone()).await
}

/// The object is gone (`ESTALE`, twice, on this filesystem's handles): the user deleted it after
/// it left (§5), and its item is deleted as any delete is — a folder only once what left it since
/// is local where it went, or gone too.
async fn gone(e: &Arc<Engine>, disk: &Disk, row: &OutboxRow, id: &str) -> Result<Outcome, Fail> {
    let folder = e.store().with(|s| s.get(Table::Items, id))?.is_some_and(|item| item.kind == Kind::Folder);
    if folder {
        let inside = inside_of(e, id)?;
        let extra = match left_since(e, disk, row, id, &inside, &HashSet::new(), last_place(row)).await? {
            Ok(extra) => extra,
            Err(outcome) => return Ok(outcome),
        };
        if let Some(outcome) = superseded(e, row, id)? {
            return Ok(outcome);
        }
        e.set_marker(row, Some(CONTENT_LOCAL))?;
        super::steps::blocking(move || {
            for file in &extra {
                strip(file)?;
            }
            Ok(())
        })
        .await?;
    }
    finish(e, row).await
}

/// Whether the row's object still stands where it was last proved to be, inside another account's
/// folder: the name there has the row's handle.
fn stands_in_another_folder(mo: &MoveOuts, disk: &Disk, row: &OutboxRow, handle: &FileHandle) -> bool {
    let Some(place) = last_place(row).filter(|p| in_another_folder(mo, disk, p)) else { return false };
    let (Some(parent), Some(name)) = (place.parent(), place.file_name()) else { return false };
    reopen_parent(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()).as_ref() == Some(handle)
}

/// The object is gone, and was last proved to be inside another account's folder (final review
/// I5), or stands there taken for that account's own (m5): that account may have removed it as
/// none of its own, or the user deleted it there. Nothing is deleted in OneDrive: the row goes, the
/// item and what is inside it forget their local objects, and the reconcile places them again in
/// this folder.
async fn kept(e: &Arc<Engine>, row: &OutboxRow, id: &str) -> Result<Outcome, Fail> {
    let event = e.event(super::kind::RESTORED, &row.rel, "it was last in another account's folder, and stays in OneDrive");
    {
        let _tree = e.cfg.tree_lock.lock().await;
        e.store().with(|s| s.outbox_drop(row.seq, None, Some(id), Some(&event)))?;
    }
    tracing::info!("{} went from another account's folder: it stays in OneDrive, and comes back here", row.rel.display());
    e.cfg.host.activity(&event);
    e.cfg.host.full_cycle_wanted();
    Ok(Outcome::Done)
}

/// The directory at `path`, by path, through the user's own lookups and no symlink.
fn reopen_parent(path: &Path) -> io::Result<File> {
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    Ok(File::from(openat2(nix::fcntl::AT_FDCWD, path, how)?))
}

fn parent_has(parent: &File, name: &OsStr) -> bool {
    nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok()
}

/// Removes the placeholder `file` at `path` in the Trash, and its entry's `.trashinfo` when it is
/// the entry itself: by name, in its directory opened by path, and only while that name is still
/// this inode. Whether it was unlinked.
fn remove(file: &File, path: &Path, entry: Option<&TrashEntry>) -> io::Result<bool> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Ok(false) };
    let parent = reopen_parent(parent)?;
    let removed = remove_at(file, &parent, name)?;
    if let Some(entry) = entry.filter(|e| removed && path == e.top) {
        remove_info(entry);
    }
    Ok(removed)
}

/// Unlinks `name` in `parent` if it is `file`: whether it did.
fn remove_at(file: &File, parent: &File, name: &OsStr) -> io::Result<bool> {
    let meta = file.metadata()?;
    match nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(there) if (there.st_dev, there.st_ino) == (meta.dev(), meta.ino()) => {
            nix::unistd::unlinkat(parent.as_fd(), name, nix::unistd::UnlinkatFlags::NoRemoveDir)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Removes directory `name` in `parent` if it is `dir` and empty: best effort.
fn remove_empty_dir(dir: &File, parent: &File, name: &OsStr) {
    let Ok(meta) = dir.metadata() else { return };
    if let Ok(there) = nix::sys::stat::fstatat(parent.as_fd(), name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        if (there.st_dev, there.st_ino) == (meta.dev(), meta.ino()) {
            let _ = nix::unistd::unlinkat(parent.as_fd(), name, nix::unistd::UnlinkatFlags::RemoveDir);
        }
    }
}

fn remove_info(entry: &TrashEntry) {
    if let (Some(dir), Some(name)) = (entry.info.parent(), entry.info.file_name()) {
        if let Ok(dir) = reopen_parent(dir) {
            let _ = nix::unistd::unlinkat(dir.as_fd(), name, nix::unistd::UnlinkatFlags::NoRemoveDir);
        }
    }
}

// ---------------------------------------------------------------------------
// Dropped rows 
// ---------------------------------------------------------------------------

/// Drops every `move-out` row, each item and what is inside it forgetting its local object — the
/// object outside, which [`Tidy::dropped`] tidies next — so that a read-write folder's reconcile
/// places it again. The rows dropped. One whose item the base has under a
/// temporary name stays, as `outbox_drop_all` keeps it.
pub(in crate::sync) fn drop_rows(s: &mut TreeStore) -> Result<Vec<OutboxRow>, TreeError> {
    let mut rows = Vec::new();
    for row in s.outbox_rows()?.into_iter().filter(|r| r.kind == OutboxKind::MoveOut) {
        let swapping = match row.item_id.as_deref() {
            Some(id) => s.get(Table::Items, id)?.is_some_and(|item| item.name.starts_with(super::SWAP_PREFIX)),
            None => false,
        };
        if !swapping {
            s.outbox_drop(row.seq, None, row.item_id.as_deref(), None)?;
            rows.push(row);
        }
    }
    Ok(rows)
}

/// What tidies after dropped `move-out` rows, with or without a worker.
pub(in crate::sync) struct Tidy<'a> {
    pub mo: &'a MoveOuts,
    pub root: &'a SyncRoot,
    pub store: &'a Store,
    pub locks: &'a InodeLocks,
}

impl Tidy<'_> {
    /// `rows` were dropped (`RestoreDeletes`, a switch to read-only, a Forget, a Remove): nothing
    /// will download what their `move-out`s left outside the folder, and nothing marks it again
    /// after the helper restarts, so each object still outside every folder is tidied as the
    /// Trash case, without the delete (the examination). Its item stays in OneDrive, and a read-write
    /// folder's reconcile places it again (the drop forgot its local object). A placeholder
    /// outside, which holds nothing whole, goes, so that it never reads as zeros (Z3); a
    /// downloaded file stays, stripped, as the user's own; a directory of the item is unmarked,
    /// stripped, and removed if left empty. Anything not proved to be outside, a placeholder with
    /// another link or being filled, and anything the helper cannot reach now, is left as it is.
    /// Local only: nothing is sent.
    pub(in crate::sync) async fn dropped(&self, rows: &[OutboxRow]) {
        let Ok(disk) = Disk::open(self.root, false) else { return };
        let Ok(root) = disk.dir(Path::new("")) else { return };
        for row in rows.iter().filter(|r| r.kind == OutboxKind::MoveOut) {
            let (Some(id), Some(handle)) = (row.item_id.as_deref(), row.inode.as_ref().and_then(|i| i.handle.as_ref())) else { continue };
            let object = match self.mo.helper.open_by_handle(&root, handle).await {
                Ok(object) => File::from(object),
                Err(HelperError::Refused(libc::ESTALE | libc::EPERM)) => continue,
                Err(err) => {
                    tracing::warn!("what left the folder as {} is left as it is, not reached: {err}", row.rel.display());
                    continue;
                }
            };
            if item_id_of(&object).as_deref() != Some(id) {
                continue;
            }
            let Some(path) = verified_path(&object).filter(|p| !beneath_a_root(self.mo, &disk, p)) else { continue };
            let entry = trash_of(&path, self.mo.home_trash.as_deref(), nix::unistd::geteuid().as_raw(), &is_mount_point).filter(real_trash);
            match self.tidy(&disk, id, object, &path, entry.as_ref()).await {
                Ok(()) => tracing::info!("{} stays in OneDrive: what had left the folder is tidied at {}", row.rel.display(), path.display()),
                Err(err) => tracing::warn!("what left the folder as {} is left as it is: {err}", row.rel.display()),
            }
        }
    }

    async fn tidy(&self, disk: &Disk, id: &str, object: File, path: &Path, entry: Option<&TrashEntry>) -> io::Result<()> {
        let mut inside: HashSet<String> =
            self.store.with(|s| s.descendants(Table::Items, id)).map_err(|_| io::Error::other("the base cannot be read"))?.into_iter().collect();
        inside.insert(id.to_owned());
        let (locks, at, trash) = (self.locks.clone(), path.to_path_buf(), entry.cloned());
        // The files, off the runtime.
        let walked = off(move || {
            if !object.metadata()?.is_dir() {
                let Some(_inode) = locks.try_lock(InodeKey::of(&object)?) else { return Ok(None) };
                match placeholder::read_state(&object) {
                    Ok(Some(State::Hydrated)) => strip(&object)?,
                    Ok(Some(_)) if object.metadata()?.nlink() == 1 => {
                        remove(&object, &at, trash.as_ref())?;
                    }
                    _ => {}
                }
                return Ok(None);
            }
            let Some(top) = reopen_dir(&at, &object)? else { return Ok(None) };
            let met = walk(&top)?;
            for m in met.iter().filter(|m| !m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))) {
                let file = open_met(&top, m)?;
                let Some(_inode) = locks.try_lock(InodeKey::of(&file)?) else { continue };
                match placeholder::read_state(&file) {
                    Ok(Some(State::Hydrated)) => strip(&file)?,
                    // Not whole: never filled, or a fill or a free-up cut short. OneDrive has it.
                    Ok(Some(_)) if file.metadata()?.nlink() == 1 => {
                        if let Some(name) = m.rel.file_name() {
                            remove_at(&file, &dir_below(&top, m.dir())?, name)?;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some((top, met, inside)))
        })
        .await?;
        let Some((top, met, inside)) = walked else { return Ok(()) };
        for m in met.iter().rev().filter(|m| m.is_dir && m.id.as_ref().is_some_and(|i| inside.contains(i))) {
            let dir = open_met(&top, m)?;
            unmark(self.mo, disk, &dir).await;
            let parent = dir_below(&top, m.dir())?;
            let name = m.rel.file_name().map(OsStr::to_os_string);
            off(move || {
                strip(&dir)?;
                if let Some(name) = name {
                    remove_empty_dir(&dir, &parent, &name);
                }
                Ok(())
            })
            .await?;
        }
        unmark(self.mo, disk, &top).await;
        let (at, trash) = (path.to_path_buf(), entry.cloned());
        off(move || {
            strip(&top)?;
            if std::fs::read_dir(proc_path(&top))?.next().is_none() {
                if let (Some(parent), Some(name)) = (at.parent().and_then(|p| reopen_parent(p).ok()), at.file_name()) {
                    remove_empty_dir(&top, &parent, name);
                    if let Some(entry) = trash.filter(|e| e.top == at && !parent_has(&parent, name)) {
                        remove_info(&entry);
                    }
                }
            }
            Ok(())
        })
        .await
    }
}

/// `f` on a blocking thread.
async fn off<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(io::Error::other)?
}

// ---------------------------------------------------------------------------
// the account's side
// ---------------------------------------------------------------------------

/// A fill of a moved-out object for an account: shown in `Transfers` and recorded in the activity
/// log like a fill on open.
struct AccountFill {
    sync: Weak<SyncService>,
}

#[async_trait]
impl Filler for AccountFill {
    async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), FillError> {
        let Some(sync) = self.sync.upgrade() else { return Err(FillError::Errno(libc::EIO)) };
        let Some(source) = sync.source.lock().unwrap().clone() else { return Err(FillError::Errno(libc::EIO)) };
        let shown = shown.display().to_string();
        let tracked = crate::sync::activity::Tracked::new(source, sync.report.transfers.clone(), shown.clone());
        let filled = source::hydrate_with(file.into(), &tracked, clearance).await;
        let size = tracked.fetched();
        drop(tracked);
        let answered = match filled {
            Ok(()) => Answered::Filled,
            Err(e) => Answered::Failed(e),
        };
        if let Some(event) = crate::sync::fill_event(&answered, &shown, size) {
            sync.report.activity.record(vec![event]).await;
        }
        match answered {
            Answered::Failed(e) => Err(e),
            _ => Ok(()),
        }
    }
}

impl SyncService {
    /// What this account's outbox worker needs for `move-out` rows: the helper over the account's
    /// link, fills through the account's source, the hub's router told which item ids are this
    /// account's wherever they are (`docs/design/writes.md` §8, §8.3), and every account's folder.
    pub(in crate::sync) fn move_outs(&self) -> MoveOuts {
        let (hub, me) = (Arc::downgrade(&self.hub), self.me.clone());
        let every = Arc::downgrade(&self.hub);
        MoveOuts {
            helper: Arc::new(Linked(Arc::clone(&self.link))),
            filler: Arc::new(AccountFill { sync: self.me.clone() }),
            route: Some(Arc::new(move |ids| {
                if let Some(hub) = hub.upgrade() {
                    hub.set_moved_out(&me, ids);
                }
            })),
            home_trash: home_trash(),
            roots: Arc::new(move || every.upgrade().map(|hub| hub.accounts().iter().flat_map(|a| a.folders()).collect()).unwrap_or_default()),
        }
    }

    /// What the examination says of the folder's file handles: taken again on a
    /// changed filesystem, which `LastError` says until a Full local scan finds them current.
    pub(in crate::sync) fn handles_hook(&self) -> Arc<dyn Fn(Option<String>) + Send + Sync> {
        let me = self.me.clone();
        Arc::new(move |note| {
            if let Some(service) = me.upgrade() {
                service.state.update(|s| s.handles_note = note.clone().unwrap_or_default());
            }
        })
    }

    /// The helper is back (`docs/design/writes.md` §10): what the pending `move-out` rows name is marked again
    /// before anything else the worker runs.
    pub(in crate::sync) fn outbox_helper_back(&self) {
        if let Some(outbox) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.helper_back();
        }
    }

    /// `rows` of the folder at `root` were dropped: what their `move-out`s left outside the
    /// folder is tidied ([`Tidy::dropped`]), whether or not a worker runs.
    pub(in crate::sync) async fn tidy_dropped(&self, root: &SyncRoot, store: &Store, rows: &[OutboxRow]) {
        if !rows.iter().any(|r| r.kind == OutboxKind::MoveOut) {
            return;
        }
        let mo = self.move_outs();
        Tidy { mo: &mo, root, store, locks: &self.locks }.dropped(rows).await;
    }

    /// The hub routes none of this account's item ids to it any more: its `move-out` rows are
    /// dropped. A worker started later routes its own again.
    pub(in crate::sync) fn forget_moved_out(&self) {
        self.hub.set_moved_out(&self.me, HashSet::new());
    }

    /// A Forget, or a Remove, drops the tree store and every row in it: the
    /// `move-out` rows go first, their items forgetting their local objects, and what they left
    /// outside the folder is tidied while the helper still holds the folder; the hub stops
    /// routing their ids. Dropped before they are tidied: a row kept over a placeholder already
    /// removed would read as the user's delete.
    pub(in crate::sync) async fn drop_moved_out(&self, root: &SyncRoot) {
        let store = self.store.lock().unwrap().clone();
        if let Some(store) = store {
            let dropping = store.clone();
            let dropped = tokio::task::spawn_blocking(move || dropping.with(drop_rows)).await;
            match dropped {
                Ok(Ok(rows)) => self.tidy_dropped(root, &store, &rows).await,
                Ok(Err(e)) => tracing::warn!("the moves out of the folder waiting to finish cannot be read: {e}"),
                Err(e) => tracing::warn!("the task dropping the moves out of the folder failed: {e}"),
            }
        }
        self.forget_moved_out();
    }

    /// Whether another account of this daemon claims an item id (`docs/design/writes.md` §8.3), for this
    /// account's reconcile: an object carrying it is never removed ([`HelperHub::claimed_elsewhere`]).
    ///
    /// [`HelperHub::claimed_elsewhere`]: crate::sync::hub::HelperHub::claimed_elsewhere
    pub(in crate::sync) fn claims(&self) -> crate::sync::materialize::Claimed {
        let (hub, me) = (Arc::downgrade(&self.hub), self.me.clone());
        Arc::new(move |id| hub.upgrade().is_some_and(|hub| hub.claimed_elsewhere(&me, id)))
    }
}
