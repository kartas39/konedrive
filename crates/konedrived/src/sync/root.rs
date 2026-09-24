//! Root registration, dehydration and startup recovery: the daemon's side of
//! binding an empty local folder to the signed-in drive, of freeing a
//! hydrated file's space again, and of cleaning up after a crash
//! that caught a file mid-operation.
//!
//! # One descriptor, from the first open to the last write
//!
//! Dehydration and recovery are the only things in this project that destroy
//! a file's contents on purpose, so everything they decide and everything
//! they do must be about the same inode. Dehydration opens the file
//! **once**, `O_RDWR`, and every
//! step after that — reading the state, checking the stamp, marking it
//! `dehydrating`, handing the descriptor to the helper for `ClearIgnore`,
//! taking the write lease, punching, restoring the mtime — goes through that
//! one descriptor. `konedrive_fs`'s API is descriptor-based throughout;
//! nothing here needs a path once the file is open.
//!
//! The version this replaces opened the path four times and punched the
//! fourth, having checked the third. A rename landing in that gap — an
//! editor's save-and-replace, `mv`, anything — meant the guard passed on the
//! old inode while `fallocate` emptied the *new* one: measured, 300 KiB of a
//! freshly written file zeroed and stamped `online-only`, three runs out of
//! three, with `dehydrate` returning `Ok(())`. A descriptor cannot be
//! renamed out from under its holder, which is the whole of the fix.
//!
//! [`recover`] walks a whole tree instead of taking one path, so it extends
//! the same rule to directories: every name it looks at is opened with
//! `openat` from a directory descriptor it already holds, and the descriptor
//! it classified is the descriptor it punches. Its own version of the defect
//! above was measured too — a `sub/` swapped for a symlink out of the root
//! while recovery awaited an ack, and a file elsewhere on the filesystem
//! emptied and counted as a success.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::{
    punch_all, punch_from, read_progress, read_stamp, read_state, remove_progress, remove_stamp, stamp_matches,
    write_state, State, StateError, XATTR_ROOT,
};
use konedrive_fs::probe::{probe_dir, ProbeError};
use konedrive_fs::MAX_DEPTH;
use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::Mode;
use xattr::FileExt;

use super::helper::{Clearance, HelperError, HelperLink, NotCleared};
use super::{InodeKey, InodeLocks};

#[derive(Debug, Clone)]
pub struct SyncRoot {
    pub path: PathBuf,
    pub root_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("not a directory")]
    NotADirectory,
    #[error("the folder must be empty")]
    NotEmpty,
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Helper(String),
}

/// The path of an open descriptor, for the few APIs that still take one.
///
/// Using `/proc/self/fd/<n>` rather than the caller's path string is the same
/// idiom the helper uses in `check_filesystem`: whatever is done through it
/// lands in the exact directory the descriptor was opened on, and cannot be
/// redirected by swapping a component of the path afterwards.
fn proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Opens a candidate sync root, and nothing else: `O_DIRECTORY` so a file
/// can never be mistaken for one, `O_NOFOLLOW` so the last component cannot
/// be a symlink pointing somewhere else entirely.
fn open_root_dir(path: &Path) -> Result<File, RegisterError> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    match nix::fcntl::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::ENOTDIR | Errno::ENOENT | Errno::ELOOP) => Err(not_a_directory(path)),
        Err(e) => Err(RegisterError::Unsupported(format!("{}: {e}", path.display()))),
    }
}

/// Why the open could not produce a directory, said in the user's terms.
///
/// The errno alone cannot: `O_DIRECTORY | O_NOFOLLOW` against a symlink —
/// even one pointing at a perfectly good directory — reports **`ENOTDIR`** on
/// Linux rather than the `ELOOP` `O_NOFOLLOW` documents on its own, so "not a
/// directory" would be the message for a folder the user can see is there.
/// One `lstat` separates them.
fn not_a_directory(path: &Path) -> RegisterError {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => RegisterError::Unsupported(format!(
            "{}: a sync root must be a real directory, not a symbolic link",
            path.display()
        )),
        _ => RegisterError::NotADirectory,
    }
}

/// Everything about a candidate folder that can be decided locally.
///
/// The write half of the probe (`probe_dir`, `konedrive_fs::probe`) is
/// authoritative *here*, not on the helper's side. The helper runs under
/// `ProtectHome=read-only`, so its own write probe can be
/// refused (`EROFS`/`EACCES`/`EPERM`) against a perfectly good directory in
/// the user's own home — that is why `check_filesystem` in
/// `konedrive-helper/src/main.rs` treats a refused write probe as "nothing
/// new learned" and falls back to `fstatfs` alone. The daemon runs
/// unprivileged, in the user's own home, with no such sandbox, so this is
/// the one place in the system where the write probe's result actually means
/// something.
pub fn check_root_candidate(path: &Path) -> Result<(), RegisterError> {
    let dir = open_root_dir(path)?;
    check_root_dir(&dir, path)
}

/// The same checks, on a directory that is already open. The probe's own
/// error text names `/proc/self/fd/<n>`, which would be meaningless to the
/// person who typed a folder name, so the message is rebuilt around the path
/// they actually gave.
///
/// # Empty is required only for *first* registration
///
/// A folder that already carries a valid `user.konedrive.root` is not a
/// candidate being registered for the first time — it is a root the daemon
/// (this run or a previous one) already claimed, and re-registering it is
/// exactly what startup recovery needs to do before it can safely touch
/// anything inside it. Such a folder is *expected* to be full: placeholders,
/// hydrated files, subdirectories, all of this daemon's own making. Refusing
/// it as `NotEmpty` — the same refusal a stranger's populated folder gets —
/// would make every restart unregister every root the moment anything had
/// been written into it, which is always, immediately after the first
/// registration. The empty check below therefore only runs when the folder
/// has no root id of its own yet; a folder that already names itself as a
/// root skips it.
///
/// # The probe runs first
///
/// `read_root_id` is a `getxattr` in the `user.*` namespace, which is the
/// very thing [`probe_dir`] exists to establish is available. Asking for the
/// root id first means that on a filesystem without `user.*` xattrs
/// registration fails with `cannot access user.konedrive.root: …` — an
/// errno, naming an attribute the user never heard of, about a folder the
/// message does not name — instead of the probe's purpose-built "the
/// filesystem does not support user xattrs" against the folder they typed.
/// The probe is also the more fundamental refusal: a folder that cannot hold
/// a placeholder at all cannot be a sync root whether it is empty or not.
///
/// # A folder that already carries one of our root ids is not probed again
///
/// It was probed when it was first registered, and a folder that shows
/// OneDrive is locked read-only since (W2), the folder itself included, so
/// the probe's write is refused there: no such folder came back after a
/// restart, in either mode — "cannot bring up the sync folder: Permission
/// denied" — and none could be switched to interception once the helper
/// arrived. The helper's own re-registration skips its probe for
/// the same reason. H93 still holds: the id is only *looked
/// for* first, and a folder where looking fails is probed, whose answer is
/// what is reported.
fn check_root_dir(dir: &File, path: &Path) -> Result<(), RegisterError> {
    if matches!(read_root_id(dir), Ok(Some(_))) {
        return Ok(());
    }
    let through_fd = proc_path(dir);
    probe_dir(&through_fd).map_err(|e| match e {
        ProbeError::Missing { feature, .. } => RegisterError::Unsupported(format!(
            "{}: the filesystem does not support {feature}",
            path.display()
        )),
        ProbeError::Unusable { why, .. } => {
            RegisterError::Unsupported(format!("{}: {why}", path.display()))
        }
    })?;
    if read_root_id(dir)?.is_none() {
        let mut entries = std::fs::read_dir(&through_fd)
            .map_err(|e| RegisterError::Unsupported(format!("{}: {e}", path.display())))?;
        if entries.next().is_some() {
            return Err(RegisterError::NotEmpty);
        }
    }
    Ok(())
}

/// Binds an empty folder to the account: probe, stamp it, tell the helper.
///
/// # Why the root id is read before it is minted
///
/// `user.konedrive.root` is written before the helper is told about it, and
/// it is never overwritten. That ordering is not free to change — a crash,
/// or a `HelperError::Timeout`, between the helper saving the root and this
/// function returning leaves a registration the daemon cannot see — so the
/// xattr is read *first* and reused whenever it is there. The helper treats
/// a registration with the same uid and the same id as idempotent,
/// so retrying with the id already on the folder converges.
///
/// Minting a fresh id on every attempt, as this used to, does the opposite:
/// the retry offers root B for a directory the helper already holds as root
/// A, which is `nesting_conflict` → `SameDirectory` → **`EINVAL`**, forever,
/// and `unregister_root(B)` can never remove A either. The folder becomes
/// permanently unregisterable. A reused id costs nothing when the first
/// attempt failed for good (an unwritable folder, a refused filesystem):
/// the id is a name, not a claim, and the next successful registration uses
/// it. We do not remove it on failure precisely because a timeout cannot
/// tell us whether the helper saved it.
///
/// `link.register_root` triggers a full `openat2` walk of the whole tree
/// inside the helper (bounded by `HelperLink`'s 120 s `register_root`
/// timeout, not the ordinary 30 s call bound). A root that walk could not
/// fully cover is tracked by the helper as "degraded"
/// (`konedrive-helper/src/main.rs::record_walk`/`degraded_roots`), but that
/// status is not yet surfaced anywhere: `RegisterRoot`'s ack carries only an
/// errno, so a degraded root still acks success here. Nothing in
/// this crate today has anywhere to put that signal — the natural home is a
/// later helper→daemon query (or an addition to `RootState`/`LastError` on
/// the `org.konedrive.Sync1` D-Bus surface), once one exists.
pub async fn register_root(link: &HelperLink, path: &Path) -> Result<SyncRoot, RegisterError> {
    let (dir, root) = prepare(path).await?;
    link.register_root(&dir, &root.root_id)
        .await
        .map_err(|e| RegisterError::Helper(e.to_string()))?;
    Ok(root)
}

/// Everything [`register_root`] does before the helper is told: the folder
/// checks, and the root id read or minted. Returns the open
/// directory the helper is to be handed, so that the descriptor the checks
/// ran on is the descriptor it registers.
///
/// Separate so that `SyncService` can write a new root down in
/// `config.toml` between the two halves — before the helper holds anything
/// the daemon's next start would not know about.
pub(super) async fn prepare(path: &Path) -> Result<(File, SyncRoot), RegisterError> {
    // Opening, listing, probing and stamping a directory are
    // all blocking syscalls, and `sync/helper.rs`'s module doc treats a
    // blocking call left on a tokio worker as a first-class defect.
    let requested = path.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_root(&requested))
        .await
        .map_err(|e| RegisterError::Unsupported(format!("the registration task failed: {e}")))?
}

/// The root id `path` carries, if it is a directory carrying one of ours —
/// read, never minted, and never followed through a symlink. For a root
/// restored from a `config.toml` that did not record its id; `None` for
/// anything that cannot be read.
pub(super) async fn recorded_root_id(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let dir = nix::fcntl::open(&path, flags, Mode::empty()).map(File::from).ok()?;
        read_root_id(&dir).ok().flatten()
    })
    .await
    .ok()
    .flatten()
}

/// [`register_root`] with nobody to intercept anything: the
/// same local checks and the same root id, but no helper is told, so no
/// directory under this root is ever marked and no open inside it is ever
/// suspended.
///
/// This is the deliberate, separately-named opt-in behind
/// `org.konedrive.Sync1.RegisterRootWithoutInterception`, never a fallback
/// that a failed helper connection can slide into: `RegisterRoot` itself
/// still refuses outright without a helper, because a placeholder nobody
/// intercepts reads as zeros, which is the one outcome this project exists
/// to prevent. What makes the mode safe to offer at all is that it is
/// *visible* — `RootState` reports `no-interception` and `LastError` says
/// plainly that files here read as zeros until they are hydrated. A folder
/// registered this way while no helper was connected is switched to
/// interception by `SyncService` once one connects.
pub async fn register_root_unprotected(path: &Path) -> Result<SyncRoot, RegisterError> {
    let (_dir, root) = prepare(path).await?;
    Ok(root)
}

/// The local half of [`register_root`]: everything up to, but not including,
/// telling the helper. Returns the open directory the helper is handed, so
/// that the descriptor the checks ran on is the descriptor it registers.
fn prepare_root(path: &Path) -> Result<(File, SyncRoot), RegisterError> {
    let dir = open_root_dir(path)?;
    check_root_dir(&dir, path)?;
    let resolved = resolved_path(&dir, path)?;
    let root_id = root_id_of(&dir)?;
    Ok((dir, SyncRoot { path: resolved, root_id }))
}

/// Where the open directory actually is, read back from the kernel rather
/// than taken from the caller's string, and then proved to still lead to the
/// same inode. The resolved path is what every later `dehydrate` measures
/// "inside this root" against, so it must be free of symlinks and `..`.
fn resolved_path(dir: &File, path: &Path) -> Result<PathBuf, RegisterError> {
    let unsupported = |e: io::Error| RegisterError::Unsupported(format!("{}: {e}", path.display()));
    let resolved = std::fs::read_link(proc_path(dir)).map_err(unsupported)?;
    let here = dir.metadata().map_err(unsupported)?;
    let there = std::fs::metadata(&resolved).map_err(unsupported)?;
    if (here.dev(), here.ino()) != (there.dev(), there.ino()) {
        return Err(RegisterError::Unsupported(format!(
            "{} moved while it was being registered",
            path.display()
        )));
    }
    Ok(resolved)
}

/// The folder's own root id, minted only if it has none. An
/// empty value is treated as none: it names no registration the helper could
/// be holding, so there is nothing to preserve.
fn root_id_of(dir: &File) -> Result<String, RegisterError> {
    if let Some(id) = read_root_id(dir)? {
        return Ok(id);
    }
    let unsupported =
        |e: io::Error| RegisterError::Unsupported(format!("cannot access {XATTR_ROOT}: {e}"));
    let root_id = uuid_v4();
    dir.set_xattr(XATTR_ROOT, root_id.as_bytes()).map_err(unsupported)?;
    Ok(root_id)
}

/// The folder's own root id, if it already carries a valid one — `None`
/// otherwise. Shared by [`check_root_dir`] (only a folder with
/// *no* id yet must be empty) and [`root_id_of`] (an existing id
/// is reused, never overwritten).
///
/// # A value that is not an id of ours is not an id
///
/// Honouring any non-empty string here is what makes
/// `setfattr -n user.konedrive.root -v x ~/Documents` enough to register a
/// folder full of somebody's existing data: H78's relaxation skips the empty
/// check for anything that "carries a root id", and nothing ever removes the
/// xattr again, so one `setfattr` disarms that check for that folder
/// permanently. It matters because §4.3's populate skips names that already
/// exist — those files never get an item id, never get a placeholder, and
/// yet live inside a tree the helper now marks and this module now walks and
/// punches.
///
/// So the shape is checked before the value is believed: the 36-character
/// `8-4-4-4-12` hex form with version nibble `4` that [`uuid_v4`] mints and
/// `uuid_v4_mints_a_fresh_identifier_every_time` spells out. Anything else —
/// absent, empty, a word, a truncated id — reads as *no* id: the folder must
/// then be empty to be registered, and [`root_id_of`] mints a real one over
/// the top. Overwriting is safe precisely because the value is not one we
/// could ever have minted, so no helper registration can be named by it
/// ("never overwrite" protects ids we *did* mint).
fn read_root_id(dir: &File) -> Result<Option<String>, RegisterError> {
    let unsupported =
        |e: io::Error| RegisterError::Unsupported(format!("cannot access {XATTR_ROOT}: {e}"));
    let Some(raw) = dir.get_xattr(XATTR_ROOT).map_err(unsupported)? else {
        return Ok(None);
    };
    let id = String::from_utf8(raw)
        .map_err(|_| RegisterError::Unsupported(format!("{XATTR_ROOT} is not valid UTF-8")))?;
    Ok(looks_like_a_root_id(&id).then_some(id))
}

/// The canonical form of what [`uuid_v4`] mints — 36 characters,
/// `8-4-4-4-12`, lowercase-or-uppercase hex throughout, version nibble `4`.
pub(super) fn looks_like_a_root_id(id: &str) -> bool {
    if id.len() != 36 {
        return false;
    }
    let fields: Vec<&str> = id.split('-').collect();
    if fields.iter().map(|f| f.len()).ne([8, 4, 4, 4, 12]) {
        return false;
    }
    if !fields.iter().all(|f| f.chars().all(|c| c.is_ascii_hexdigit())) {
        return false;
    }
    fields[2].starts_with('4')
}

/// 16 random bytes in the canonical form; no dependency for one identifier.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the OS random number generator failed");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[derive(Debug, thiserror::Error)]
pub enum DehydrateError {
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("the file is not downloaded")]
    NotHydrated,
    #[error("the file was modified locally")]
    ModifiedLocally,
    #[error("the file is in use")]
    InUse,
    #[error("not a plain file inside this sync root")]
    OutsideRoot,
    /// A helper is running and this daemon has no link to it, so a mark its
    /// group may hold on the file cannot be cleared. Nothing
    /// was changed; try again once the link is up.
    #[error("the konedrive helper is running but not connected to this daemon")]
    HelperNotConnected,
    #[error("{0}")]
    Io(String),
}

fn io_error(e: impl std::fmt::Display) -> DehydrateError {
    DehydrateError::Io(e.to_string())
}

impl From<NotCleared> for DehydrateError {
    fn from(e: NotCleared) -> Self {
        match e {
            NotCleared::Unlinked => DehydrateError::HelperNotConnected,
            other => io_error(other),
        }
    }
}

impl SyncRoot {
    /// Opens `path` for dehydration: once, `O_RDWR`, and only if it really
    /// is a plain file inside this root. A file the read-only
    /// lock made `0444` refuses `O_RDWR`; it is opened read-only instead and
    /// reopened writable on the same inode, and only when it is one of ours
    /// (see [`open_locked`](Self::open_locked)).
    ///
    /// Three separate things are checked, because the punch that follows is
    /// irreversible:
    ///
    /// - the root directory still carries *this* root's `user.konedrive.root`
    ///   — a registration that has been removed or replaced no longer
    ///   authorises emptying anything inside it;
    /// - the file's path lies under the root's resolved path, with its parent
    ///   directory resolved first so that `..` and symlinked parents cannot
    ///   spell their way out;
    /// - the kernel agrees, which is what `RESOLVE_BENEATH` is for: the open
    ///   is refused outright if resolution would leave the root, and
    ///   `RESOLVE_NO_SYMLINKS` plus `O_NOFOLLOW` refuse it if any component,
    ///   including the last, is a symlink at all.
    ///
    /// The helper's own check cannot stand in for this one: it is scoped to
    /// the device a root lives on, not to the root itself, and it is not
    /// consulted here anyway. It matters as soon as puts a path from
    /// outside this process on the other end of a D-Bus method.
    ///
    ///: this is the *only* way anything in `sync` may turn a
    /// caller's path into a descriptor it will write through.
    /// `SyncService::hydrate_now` used to open the checked string itself,
    /// with no `O_NOFOLLOW` and no `RESOLVE_BENEATH`, after awaiting an
    /// unbounded lock — measured, a file outside the root overwritten with
    /// hydration content and `Hydrate` reporting success. An ordinary
    /// directory rename inside the root was enough; no attacker was required.
    pub(super) fn open_inside(&self, path: &Path) -> Result<File, DehydrateError> {
        let dir = self
            .open_registered()
            .map_err(|e| DehydrateError::Io(format!("{}: {e}", self.path.display())))?
            .ok_or(DehydrateError::OutsideRoot)?;

        let relative = self.relative(path)?;
        let how = OpenHow::new()
            .flags(OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            );
        match openat2(dir.as_fd(), &relative, how) {
            Ok(fd) => Ok(File::from(fd)),
            Err(Errno::EACCES) => self.open_locked(&dir, &relative, path),
            Err(Errno::EXDEV | Errno::ELOOP | Errno::EISDIR) => Err(DehydrateError::OutsideRoot),
            Err(e) => Err(DehydrateError::Io(format!("{}: {e}", path.display()))),
        }
    }

    /// [`open_inside`](Self::open_inside) for a file the read-only lock made
    /// `0444`: opened for reading with the same resolution rules,
    /// and reopened writable — on the same inode — only if it carries a
    /// konedrive state. A file that is not ours comes back read-only and
    /// untouched; every caller refuses such a file before it writes anything.
    ///
    /// `O_NONBLOCK`, because a read-only open of a FIFO waits for a writer,
    /// and a `0444` FIFO is refused `O_RDWR` and so reaches here; the `fstat`
    /// below then refuses it.
    fn open_locked(&self, dir: &File, relative: &Path, shown: &Path) -> Result<File, DehydrateError> {
        let how = OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            );
        let read_only = match openat2(dir.as_fd(), relative, how) {
            Ok(fd) => File::from(fd),
            Err(Errno::EXDEV | Errno::ELOOP | Errno::EISDIR) => return Err(DehydrateError::OutsideRoot),
            Err(e) => return Err(DehydrateError::Io(format!("{}: {e}", shown.display()))),
        };
        if !read_only.metadata().map_err(io_error)?.is_file() {
            return Err(DehydrateError::OutsideRoot);
        }
        match read_state(&read_only) {
            Ok(Some(_)) => {
                let writable = konedrive_fs::placeholder::reopen_writable(&read_only).map_err(io_error)?;
                // Only the writable descriptor may be left: a second one of
                // our own would refuse the write lease a free-up takes.
                drop(read_only);
                Ok(writable)
            }
            _ => Ok(read_only),
        }
    }

    /// This root's directory, opened as a directory and proved to still be
    /// *this* registered root — `Ok(None)` when the folder no longer carries
    /// this root's `user.konedrive.root`.
    ///
    /// Every descriptor-based walk into the root starts here, because a
    /// registration that has been removed or replaced (the user unregistered
    /// the folder, something else claimed it) no longer authorises emptying
    /// anything inside it. `O_DIRECTORY` so a file can never be mistaken for
    /// the root, `O_NOFOLLOW` so its last component cannot be a symlink to
    /// somewhere else entirely.
    pub(super) fn open_registered(&self) -> io::Result<Option<File>> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let dir = nix::fcntl::open(&self.path, flags, Mode::empty()).map(File::from)?;
        match dir.get_xattr(XATTR_ROOT)? {
            Some(id) if id == self.root_id.as_bytes() => Ok(Some(dir)),
            _ => Ok(None),
        }
    }

    /// `path` as a name to resolve from this root's directory descriptor.
    /// The parent is resolved first and the final component is carried over
    /// untouched, so a symlinked file is refused by the open rather than
    /// silently followed here.
    pub(super) fn relative(&self, path: &Path) -> Result<PathBuf, DehydrateError> {
        let name = path.file_name().ok_or(DehydrateError::OutsideRoot)?;
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let parent = std::fs::canonicalize(parent).map_err(io_error)?;
        let root = std::fs::canonicalize(&self.path).map_err(io_error)?;
        let inside = parent.strip_prefix(&root).map_err(|_| DehydrateError::OutsideRoot)?;
        Ok(inside.join(name))
    }
}

/// The guard: only a clean, fully downloaded file may be emptied
/// (dehydration's step 1). It runs on the very descriptor the punch will
/// use, so what it decided about cannot be swapped for something else
/// afterwards.
pub fn check_dehydratable(file: &File) -> Result<(), DehydrateError> {
    match read_state(file).map_err(io_error)? {
        None => Err(DehydrateError::NotManaged),
        Some(State::Hydrated) => {
            if stamp_matches(file).map_err(io_error)? {
                Ok(())
            } else {
                Err(DehydrateError::ModifiedLocally)
            }
        }
        Some(_) => Err(DehydrateError::NotHydrated),
    }
}

/// Whether `file` is a zero-byte file as `create_placeholder` makes one:
/// `hydrated` from birth, with no stamp, since there is nothing to download.
/// Freeing it up has nothing to free, and succeeds without touching it
/// — it used to fail the stamp check and answer
/// `ModifiedLocally`, which told the user their edits would be lost. A file
/// that *became* empty here still carries the stamp of what it held, fails
/// that check, and is refused as modified.
fn nothing_to_free(file: &File) -> Result<bool, DehydrateError> {
    if read_state(file).map_err(io_error)? != Some(State::Hydrated) {
        return Ok(false);
    }
    let empty = file.metadata().map_err(io_error)?.len() == 0;
    Ok(empty && read_stamp(file).map_err(io_error)?.is_none())
}

/// The timestamps a punch would destroy, kept so they can be put back
///.
#[derive(Clone, Copy)]
struct FileTimes {
    atime: libc::timespec,
    mtime: libc::timespec,
}

impl FileTimes {
    fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            atime: libc::timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() },
            mtime: libc::timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() },
        })
    }

    /// Puts both back on the same descriptor. `futimens` takes the two raw
    /// `timespec`s the file was carrying, so a pre-epoch or
    /// nanosecond-precise mtime survives the round trip exactly.
    fn restore(self, file: &File) -> io::Result<()> {
        let times = [self.atime, self.mtime];
        // SAFETY: `file` is an open descriptor and `times` is a live array of
        // exactly the two `timespec`s `futimens` reads.
        let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Dehydration's (`docs/design/hydration.md` §8) steps 1–2, first half:
/// check, then publish `dehydrating` and make
/// it durable *before* anyone is told to stop intercepting this file.
///
/// # Why this cannot be moved after `ClearIgnore`
///
/// The order is the difference between failing loudly and failing silently.
/// With the mark cleared and the state still reading `hydrated`, an open
/// landing in that window reaches the helper's `handle_open`, matches
/// `Ok(Some(State::Hydrated))`, and is answered by **placing a fresh ignore
/// mark and allowing** — and then we punch. The file ends up empty *and*
/// permanently un-intercepted: zeros, forever, with no `ClearIgnore` failure
/// anywhere to notice. The lease does not cover that window; it is taken
/// afterwards.
///
/// With `dehydrating` already on disk the same open takes the `hydrate(...)`
/// arm instead: it waits, and re-hydrates after we finish, which is exactly
/// what §8's closing paragraph promises. The `fsync` is what makes the new
/// state survive a crash in the middle of the sequence, where startup
/// recovery (§4.4) then finds a `dehydrating` file and cleans it up.
/// # The barrier's own failure rolls back, like the two either side
///
/// If the `fsync` fails, the state write before it still happened — in page
/// cache, on a file that is otherwise exactly as hydrated as it was a moment
/// ago. Returning `Err` and leaving `dehydrating` behind would announce a
/// dehydration that never started, and the next startup recovery would empty
/// a perfectly good, fully hydrated file on the strength of it. So this
/// failure rolls the state back to `hydrated` exactly as a refused
/// `ClearIgnore` and a refused lease do.
fn mark_dehydrating(file: &File) -> Result<FileTimes, DehydrateError> {
    check_dehydratable(file)?;
    // Captured before anything touches the file: `fallocate` bumps the mtime
    // to now, and §4.2 requires an `online-only` file's mtime to still be the
    // remote `lastModifiedDateTime` that `create_placeholder` set.
    let times = FileTimes::of(file).map_err(io_error)?;
    write_state(file, State::Dehydrating).map_err(io_error)?;
    if let Err(e) = file.sync_all() {
        return Err(roll_back(file, io_error(e)));
    }
    Ok(times)
}

/// Undoes [`mark_dehydrating`] when the sequence stops before the punch
/// (steps 2 and 3 both say "roll back and report").
///
/// The rollback's own failure is reported, never swallowed: it leaves a file
/// stuck in `dehydrating` with its blocks intact, which startup recovery
/// (§4.4) will punch and turn into `online-only` — correct, but a silent
/// "free up space failed" that empties the file at the next start is not
/// something to discover from a log line that was never written.
fn roll_back(file: &File, cause: DehydrateError) -> DehydrateError {
    match write_state(file, State::Hydrated) {
        Ok(()) => cause,
        Err(e) => {
            tracing::error!("cannot roll the state back to hydrated after {cause}: {e}");
            DehydrateError::Io(format!(
                "{cause}, and rolling the state back to hydrated failed: {e}"
            ))
        }
    }
}

/// Dehydration's steps 3–5: take the lease, punch, put the mtime back, publish
/// `online-only`.
///
/// The caller must have cleared the helper's ignore mark first (invariant
/// M3) — this is private for that reason: an exported function
/// that punches with no helper involvement puts the project's one silent,
/// unrecoverable failure behind nothing but a doc comment.
///
/// Only the two refusals *before* the punch roll the state back. Once
/// `fallocate` has run there is nothing to roll back to — the blocks are
/// gone and the file is not `hydrated` any more — so a failure from there on
/// leaves it `dehydrating` deliberately: startup recovery punches
/// whatever is left, sets `online-only` and removes the stamp, which is the
/// correct end state, and the next open hydrates it again.
fn punch_clean_file(file: &File, restore: FileTimes) -> Result<(), DehydrateError> {
    punch_clean_file_watched(file, restore, |_, _| {})
}

/// The two ends of the lease, as somewhere a test can stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// The lease has just been taken and nothing has been destroyed yet.
    UnderLease,
    /// The file is punched, its mtime is back, and it has been published
    /// `online-only`; the lease is about to be released.
    BeforeRelease,
}

/// As [`punch_clean_file`], with an observation point at each end of the
/// lease's life.
///
/// Both exist so that "nobody can open this file while it is being emptied"
/// is something a test can *measure* — an opener started at `UnderLease`
/// must still be suspended at `BeforeRelease` — rather than something a
/// reader has to infer from where a `drop` happens to sit. One point was not
/// enough: a lease released between the hook and the punch, or between the
/// punch and the state flip, both went unnoticed by a test that only asked
/// `F_GETLEASE` at the start. Production takes the empty
/// closure, which costs nothing.
fn punch_clean_file_watched(
    file: &File,
    restore: FileTimes,
    mut watch: impl FnMut(Watch, &File),
) -> Result<(), DehydrateError> {
    // Step 3. The kernel grants this only while nobody else holds the file
    // open, which is what makes emptying it safe; a refusal is "in use", and
    // the state must go back to `hydrated` before we report it.
    let lease = match WriteLease::take(file).map_err(io_error)? {
        Some(lease) => lease,
        None => return Err(roll_back(file, DehydrateError::InUse)),
    };

    watch(Watch::UnderLease, file);

    // Steps 4–5. The lease is held across all of them: `lease` is dropped
    // below, so every open arriving from here on waits for the break instead
    // of reading a file mid-punch or a file that is empty but still says
    // `dehydrating`.
    punch_and_publish(file, restore).map_err(io_error)?;
    watch(Watch::BeforeRelease, file);
    drop(lease);
    Ok(())
}

/// Dehydration's steps 4–5 on their own: empty the file, put the mtime back, make
/// that durable, and only then say it is `online-only`.
///
/// **The caller must hold the write lease across this call** and must have
/// confirmed the helper's `ClearIgnore` (invariant M3) before it. Both
/// [`punch_clean_file_watched`] and startup recovery's `reset_interrupted`
/// run it, because the sequence a crash left half-finished is the same
/// sequence a dehydration runs — what differs is only how the two arrive
/// here and what a refusal means to each of them, which is why the lease is
/// taken by the caller rather than in here.
fn punch_and_publish(file: &File, restore: FileTimes) -> io::Result<()> {
    punch_all(file)?;
    // Before the fsync, so the restored mtime is covered by it.
    restore.restore(file)?;
    file.sync_all()?;

    // The stamp described a hydrated file; leaving it behind would describe
    // this one wrongly, so its removal is reported rather than swallowed —
    // even though by now the file really is online-only.
    write_state(file, State::OnlineOnly)?;
    remove_stamp(file).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("the file is now online-only, but its stamp could not be removed: {e}"),
        )
    })
}

/// The full sequence, including the helper round trip.
///
/// `root` is the registered sync root the file must live in; see
/// [`SyncRoot::open_inside`] for what that is worth and why the check is
/// here.
///
/// # Why `clear_ignore`'s error must propagate — never `let _ =`
///
/// The helper's ignore mark carries `FAN_MARK_IGNORED_SURV_MODIFY` (spec
/// §5.1, invariant M3), which removed an accidental safety net: an ignore
/// mark used to be cleared by any modification to the file, so a dehydration
/// that skipped or failed the clear used to repair itself the moment the
/// punch modified the file. It no longer does. If this were ever weakened —
/// "the clear mostly works, and worst case the helper re-derives it later" —
/// a failed `ClearIgnore` followed by a punch leaves the file **empty and
/// permanently un-intercepted**: every future open is allowed straight
/// through by the stale ignore mark and reads zeros, silently, forever
/// (until the kernel happens to evict the inode). That is the worst outcome
/// this whole sub-project exists to avoid, and it would happen with no error
/// visible anywhere past this point. Do not "simplify" this into a swallowed
/// error.
///
/// The guarantee covers every *reported* failure: `Refused`, `Timeout`,
/// `NotRunning` and `Io` all arrive here as `Err` and stop the sequence, and
/// a panic propagates out of this function, which has no `catch_unwind`. It
/// cannot cover a helper that acknowledges success without having acted —
/// that is out of the daemon's reach by construction — and it is not a
/// guarantee `punch_clean_file` makes on its own, which is why that function
/// is private.
///
/// This needs no special case for "the mark was already gone": the helper
/// acks `ClearIgnore` with errno 0 both when it actually removed the mark
/// and when the mark was never there or the kernel had already evicted it
/// (an evictable mark is designed to vanish on its own) — see
/// `konedrive-helper/src/main.rs`'s `apply`/`ClearIgnore` handling and spec
/// §5.1. So any non-zero errno reaching here means something genuinely went
/// wrong, and stopping is always the right call.
///
/// # Cancelling this leaves the file `dehydrating`
///
/// Every failure *this function reports* rolls the state back or is past the
/// point where there is anything to roll back to. Dropping the future does
/// not: between `mark_dehydrating` and the punch the file reads
/// `dehydrating` on disk, and a caller that cancels there (a shutdown, a
/// `tokio::time::timeout`, a `select!` losing a race) leaves it that way,
/// with its blocks intact and its ignore mark possibly already cleared.
/// That is a safe state, not a lost one — the helper treats `dehydrating`
/// as "hydrate it again", and [`recover`] punches and relabels
/// it at the next start — but it is not a *tidy* one, so a caller that can
/// cancel should prefer to let the sequence finish.
pub async fn dehydrate(
    link: &HelperLink,
    root: &SyncRoot,
    path: &Path,
) -> Result<(), DehydrateError> {
    // The file work is blocking and belongs on a blocking
    // thread.
    let target = path.to_path_buf();
    let owned_root = root.clone();
    let file = tokio::task::spawn_blocking(move || owned_root.open_inside(&target))
        .await
        .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))??;
    dehydrate_opened(&Clearance::Link(link.clone()), file).await
}

/// [`dehydrate`] from the descriptor on, so that a caller which had to open
/// the file itself — `SyncService::dehydrate`, which needs its inode to take
/// per-inode lock *before* anything is marked —
/// hands that same descriptor straight in rather than resolving the name a
/// second time. is thereby strengthened, not weakened: there is
/// now exactly one open per dehydration, and it is the one every step uses.
///
/// # What stands between the punch and a stale ignore mark
///
/// `clearance` is step 2, decided by the local rule on
/// [`Clearance`]: with a helper link, the helper clears the mark — for a
/// folder without interception too, where it used to be skipped; with no
/// helper bound to its socket, no mark of ours exists; with a helper bound
/// and no link, nothing is emptied and the call answers
/// [`DehydrateError::HelperNotConnected`]. `SyncService::dehydrate` passes
/// the link for an intercepted root and refuses `NoHelper` without one, since
/// a file freed up there with nothing intercepting would read zeros.
pub(super) async fn dehydrate_opened(
    clearance: &Clearance,
    file: File,
) -> Result<(), DehydrateError> {
    // Each phase hands the descriptor on to the next, so all three still
    // operate on the single open of.
    let marked = tokio::task::spawn_blocking(move || {
        if nothing_to_free(&file)? {
            return Ok(None);
        }
        let restore = mark_dehydrating(&file)?;
        Ok::<_, DehydrateError>(Some((file, restore)))
    })
    .await
    .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))??;
    let Some((file, restore)) = marked else {
        return Ok(());
    };

    // Step 2, second half: local rule, here, where the file
    // is about to be emptied, and after `dehydrating` is durable (H69) — so a
    // mark an opener places from now on is placed through a `hydrate` path
    // that reads `dehydrating` and takes it off again (H139). The descriptor
    // the helper is handed is the one that was just checked and marked, and
    // the one that is about to be punched: the mark cannot be cleared on one
    // inode and a hole punched in another.
    //
    // Nothing here rests on what can or cannot have happened to the file
    // before: not on "a folder without interception carries no mark that
    // matters", which was falsified three times (H132, C2, N2), each time by
    // a race nobody had seen. A link means the helper clears the mark; no
    // helper bound to its socket means no group of ours, and so no mark,
    // exists; a helper bound and no link means nothing is emptied.
    if let Err(e) = clearance.clear(&file).await {
        return Err(tokio::task::spawn_blocking(move || roll_back(&file, DehydrateError::from(e)))
            .await
            .unwrap_or_else(|e| DehydrateError::Io(format!("the rollback task failed: {e}"))));
    }

    tokio::task::spawn_blocking(move || punch_clean_file(&file, restore))
        .await
        .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))?
}

/// How much of a registered root a startup [`recover`] found, fixed, and
/// could not reach.
///
/// `{ reset, scanned }` alone could not tell "nothing needed fixing" from
/// "every single file was refused", and the second of those leaves files in
/// `dehydrating` — invariant M3's dangerous state — waiting for a next start
/// that may never come. The four counts below are what a caller needs to see
/// that, and every file the walk meets lands in exactly one of them (or in
/// none, when it is simply not ours).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Files carrying a `user.konedrive.state` this daemon wrote: everything
    /// it manages, in whatever state the crash left it. A file with no state
    /// xattr at all is not ours and is not counted here.
    pub scanned: usize,
    /// Interrupted files (`hydrating` or `dehydrating`) that were punched
    /// back to `online-only`. A subset of `scanned`.
    pub reset: usize,
    /// Interrupted files that could **not** be reset: the helper refused the
    /// `ClearIgnore`, or the punch itself failed. Each is left exactly as it
    /// was found, for the next start to try again. A subset of `scanned`,
    /// disjoint from `reset` and `busy`.
    pub failed: usize,
    /// Things recovery could not look at at all: a directory it could not
    /// open or list, a file it could not open, a file whose state xattr it
    /// could not read, a subtree on another filesystem, or a branch deeper
    /// than [`MAX_DEPTH`]. Anything counted here may be hiding an
    /// interrupted file, so a non-zero value means the root was **not**
    /// fully recovered.
    pub skipped: usize,
    /// Interrupted files something had open, so no lease could be taken
    ///. Not a failure: whatever has the
    /// file open is, as often as not, an opener waiting for it to be filled
    /// — or, after a reconnect, a fill from the connection before, still
    /// running — and an interrupted file is one the helper
    /// fills on its next open. Left exactly as found, like a failure, for the
    /// next start if nothing fills it first. A subset of `scanned`.
    pub busy: usize,
    /// Interrupted files left exactly as found because a helper is running
    /// and this daemon has no link to it yet, so a mark its group may hold
    /// on them cannot be cleared. Not a failure: recovery runs
    /// again once the link is up (`SyncService::resume`). A subset of
    /// `scanned`.
    pub deferred: usize,
}

/// Why a whole [`recover`] could not run. Everything smaller than this —
/// one unreadable directory, one file that could not be opened, one
/// `ClearIgnore` the helper refused — is logged and counted in the
/// [`RecoveryReport`] instead, because recovery is the one component whose
/// entire job is coping with a messy on-disk state.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("{0} no longer carries this sync root's registration")]
    NotRegistered(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Why one interrupted file could not be reset. The kind is preserved rather
/// than flattened into a string, so a caller (and a log reader) can tell a
/// helper that refused from a helper that timed out from a disk that is
/// full — three situations with three different answers.
#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    #[error("the helper did not clear the ignore mark: {0}")]
    Helper(HelperError),
    /// A helper is running and there is no link to it.
    #[error("a konedrive helper is running and this daemon is not connected to it yet")]
    Unlinked,
    /// Under the lease the file no longer reads `hydrating` or
    /// `dehydrating` — something finished it after recovery first looked.
    #[error("the file was finished meanwhile; it now reads {0:?}")]
    Finished(Option<State>),
    #[error("the file is open in another process")]
    InUse,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl From<NotCleared> for ResetError {
    fn from(e: NotCleared) -> Self {
        match e {
            NotCleared::Helper(e) => ResetError::Helper(e),
            NotCleared::Unlinked => ResetError::Unlinked,
            NotCleared::Unknown(why) => ResetError::Io(io::Error::other(why)),
        }
    }
}

/// Startup recovery,: after a crash or power loss, a file caught
/// mid-hydration or mid-dehydration holds content that must not be trusted —
/// punch it back to `online-only` so the next open fetches it again.
///
/// # Why this takes a [`Clearance`], unlike 's own draft
///
/// A file that crashed `dehydrating` can still be carrying its ignore mark:
/// the daemon may have died between `write_state(Dehydrating)` and a
/// successful `ClearIgnore`. That mark carries
/// `FAN_MARK_IGNORED_SURV_MODIFY` (`docs/design/hydration.md` §4.3), so
/// nothing clears it on its own any more, and punching a file that still
/// carries it reproduces the exact defect `dehydrate` in this module exists
/// to prevent (invariant M3): the file ends up empty *and* permanently
/// un-intercepted, reading as zeros on every future open with no error
/// anywhere to notice it. So every file this function is about to punch goes
/// through the same local rule `dehydrate` applies (on
/// [`Clearance`]) — on the same descriptor it is about to punch, never a
/// path re-open — and a file the rule does not clear is left
/// exactly as it was found: counted `failed` when the helper refused, and
/// `deferred` when a helper is running that this daemon has no link to,
/// until the link is up. This needs no special case for a mark that was
/// never there or that the kernel already evicted: the helper acks that as
/// success too (see `dehydrate`'s doc comment).
///
/// For an intercepted root the caller passes the link it has just
/// registered the root on — callers must connect to the helper and register
/// their roots before recovering them, not after. For a root
/// registered without interception the caller passes whatever the rule has
/// to go on: its link if it has one, the helper's socket if not.
///
/// # The file can be finished while recovery looks at it
///
/// Recovery runs on every reconnect, and the previous connection's fills
/// keep running while it walks. It read a file's state once, when it
/// opened it, and nothing stopped a fill from committing
/// `hydrated` — and an opener from having the helper ignore-mark the file —
/// between recovery's `ClearIgnore` and its lease: recovery then punched a
/// complete file under a fresh mark, and the next reader got 65 536 zero
/// bytes (measured with that gap widened). So,
/// for each interrupted file:
///
/// - the per-inode lock every fill and every free-up of this daemon holds
///   (`locks`, `SyncService`'s own) is taken, and if it is held the file is
///   left as it is and counted `busy` — something is filling it or freeing
///   it up right now. Taken without waiting: waiting would hold the reconnect
///   behind a download of any length;
/// - the state is read **again once the lease is held**, and the file is
///   punched only if it still reads `hydrating` or `dehydrating`. Under the
///   lease nothing else has the file open, so no fill is under way; and a
///   file that came back to one of those two states after reading
///   `hydrated` did so through a free-up or a fill that cleared its mark
///   first (M3), and cannot be marked again while it reads them. A file that
///   reads anything else was finished meanwhile, and is left as it is.
///
/// # The walk never leaves the root, by construction
///
/// This empties files, in bulk, across a whole tree, with no user pointing
/// at any of them — so the containment `SyncRoot::open_inside` gives
/// `dehydrate` matters more here, not less. It takes the `&SyncRoot` rather
/// than a path for exactly that reason, and:
///
/// - the root directory must still carry *this* root's `user.konedrive.root`
///   before anything inside it is touched at all ([`RecoveryError::NotRegistered`]);
/// - every step of the walk is an `openat` from a **directory descriptor**,
///   never a path — `O_DIRECTORY | O_NOFOLLOW` for subdirectories, `O_RDONLY
///   | O_NOFOLLOW` for files, reopened writable through the descriptor itself
///   only for the one being reset — so the name that was classified is the
///   name that is opened, out of a directory that cannot be swapped
///   underneath the walk while it waits for the helper;
/// - every descriptor is `fstat`ed after it is opened and refused unless it
///   is a regular file (or a directory) on the root's own `st_dev`.
///
/// The version this replaces re-resolved every subdirectory by absolute path
/// with `std::fs::read_dir` and opened every file with `File::open`, both of
/// which follow symlinks, having classified the entry with an lstat-shaped
/// `DirEntry::file_type` an unbounded time earlier — the gap is a helper
/// round trip per interrupted file. Reproduced by the review: with `sub/`
/// replaced by a symlink to a directory outside the root while recovery
/// awaited a `ClearIgnore` ack, a file outside the root was emptied,
/// relabelled `online-only` and counted as a success. The helper does not
/// back this out: its own check is "same uid, same *filesystem* as one of
/// that uid's roots", which any file in the user's home satisfies.
///
/// # One file open at a time
///
/// The walk opens a file, decides about it, punches it and closes it before
/// it opens the next one. The version this replaces opened *every* regular
/// file in a directory `O_RDWR` — whatever its state — and held all of those
/// descriptors until the directory was finished, with `Err(_) => continue`
/// turning the inevitable `EMFILE` into silence: measured, 2000 interrupted
/// files under the default `RLIMIT_NOFILE` of 1024 gave
/// `RecoveryReport { reset: 1009, scanned: 1009 }` and left 991 files
/// `hydrating` with untrusted content, no error and no log line. What the
/// walk does hold is one directory descriptor per *level* of the tree it is
/// currently inside, which is bounded by the depth of the tree rather than
/// by the size of any directory, and a directory that cannot be opened is
/// counted in `skipped` rather than passed over in silence.
pub async fn recover(
    clearance: &Clearance,
    root: &SyncRoot,
    locks: &InodeLocks,
) -> Result<RecoveryReport, RecoveryError> {
    let opened = root.clone();
    let root_dir = on_blocking_thread(move || opened.open_registered())
        .await??
        .ok_or_else(|| RecoveryError::NotRegistered(root.path.clone()))?;
    let root_dev = root_dir.metadata()?.dev();

    let mut report = RecoveryReport::default();
    let mut stack = Vec::new();
    descend(&mut stack, Arc::new(root_dir), root.path.clone(), 0, &mut report).await;

    while let Some(mut frame) = stack.pop() {
        let Some((name, kind)) = frame.names.next() else {
            // Finished: the directory descriptor goes with the frame.
            continue;
        };
        let dir = Arc::clone(&frame.dir);
        let shown = frame.shown.join(&name);
        let child_depth = frame.depth + 1;
        // Back on the stack before anything is awaited, so the walk resumes
        // in this directory — through this descriptor — afterwards.
        stack.push(frame);

        let opened = on_blocking_thread(move || open_entry(&dir, &name, kind, root_dev)).await?;
        match opened {
            Err(e) => {
                report.skipped += 1;
                tracing::warn!("startup recovery: cannot open {}: {e}", shown.display());
            }
            Ok(Entry::Elsewhere) => {}
            Ok(Entry::OtherFilesystem) => {
                report.skipped += 1;
                tracing::warn!(
                    "startup recovery: {} is on a different filesystem than the root and was \
                     not recovered",
                    shown.display()
                );
            }
            Ok(Entry::Directory(sub)) => {
                descend(&mut stack, Arc::new(sub), shown, child_depth, &mut report).await;
            }
            Ok(Entry::File(file, state)) => {
                recover_file(clearance, locks, file, state, &shown, &mut report).await;
            }
        }
    }
    Ok(report)
}

/// One directory the walk is part-way through: the descriptor everything
/// inside it is opened from, and the names still to look at.
///
/// Names, not descriptors: a directory of 100 000 files costs one `Vec` of
/// its names, while opening them up front would cost 100 000 descriptors
///. The `FileType` beside each name is the `d_type` the kernel
/// gave us — a hint for which of the two opens to attempt, never the
/// authority for what was opened, which is the `fstat` in [`open_entry`].
struct Frame {
    dir: Arc<File>,
    /// The path a person would recognise, for log lines only. Nothing is
    /// ever opened through it.
    shown: PathBuf,
    names: std::vec::IntoIter<(OsString, std::fs::FileType)>,
    /// This directory's nesting level under the root, root itself being 0 —
    /// the same convention the helper's `walk_below` uses for `MAX_DEPTH`.
    depth: usize,
}

/// Lists `dir` and pushes it onto the walk. A directory that cannot be
/// listed is counted and left behind rather than ending the walk: a
/// mode-`000` subdirectory, or one the user deleted while the daemon
/// was starting, used to make `recover` return `Err` for the *whole* root,
/// losing the count of what it had already punched and never visiting a
/// sibling subtree.
async fn descend(
    stack: &mut Vec<Frame>,
    dir: Arc<File>,
    shown: PathBuf,
    depth: usize,
    report: &mut RecoveryReport,
) {
    if depth >= MAX_DEPTH {
        // The helper's own walk stopped marking at this depth — the same
        // `konedrive_fs::MAX_DEPTH` — so anything under here is unmarked and
        // cannot be intercepted regardless of what recovery finds in it. Counted, not
        // silent: `skipped`'s own doc comment promises that.
        report.skipped += 1;
        tracing::warn!(
            "startup recovery: {} is deeper than {MAX_DEPTH} levels, matching the helper's own \
             limit — not walked further",
            shown.display()
        );
        return;
    }
    let listing = {
        let dir = Arc::clone(&dir);
        on_blocking_thread(move || list_names(&dir)).await
    };
    match listing {
        Ok(Ok((names, unreadable))) => {
            report.skipped += unreadable;
            if unreadable > 0 {
                tracing::warn!(
                    "startup recovery: {unreadable} entries of {} could not be read",
                    shown.display()
                );
            }
            stack.push(Frame { dir, shown, names: names.into_iter(), depth });
        }
        Ok(Err(e)) | Err(e) => {
            report.skipped += 1;
            tracing::warn!(
                "startup recovery: cannot read {}, so nothing inside it is recovered this \
                 time: {e}",
                shown.display()
            );
        }
    }
}

/// Every name in an open directory, with the entry kind the kernel reported
/// for it, plus a count of the entries that could not be read at all.
///
/// The directory is listed through `/proc/self/fd/<n>`, so the listing is of
/// the descriptor the walk holds — not of whatever the path may name by now.
fn list_names(dir: &File) -> io::Result<(Vec<(OsString, std::fs::FileType)>, usize)> {
    let mut names = Vec::new();
    let mut unreadable = 0;
    for entry in std::fs::read_dir(proc_path(dir))? {
        match entry.and_then(|entry| Ok((entry.file_name(), entry.file_type()?))) {
            Ok(entry) => names.push(entry),
            Err(_) => unreadable += 1,
        }
    }
    Ok((names, unreadable))
}

/// What one name in a directory turned out to be, once opened.
enum Entry {
    /// A directory on the root's own filesystem, to walk into.
    Directory(File),
    /// A regular file on the root's own filesystem, open read-only, with
    /// whatever `user.konedrive.state` it carries — including the error of
    /// failing to make sense of it, which is emphatically not the same as
    /// carrying none (see `konedrive_fs::placeholder::StateError`).
    File(File, Result<Option<State>, StateError>),
    /// Something startup recovery has no business opening for writing: a
    /// symlink, a FIFO, a socket or a device node. None of it is ours, and
    /// none of it is worth a log line — a directory listing full of a
    /// user's ordinary files is expected to contain exactly this kind of
    /// thing, and recovery not commenting on every one of them is the point.
    Elsewhere,
    /// A directory or file `fstat`-confirmed to be on a filesystem other
    /// than the root's own `st_dev` — a bind mount or a removable disk
    /// mounted inside the sync folder. Unlike [`Entry::Elsewhere`] this is
    /// not silent: the helper's own validation is scoped to the root's
    /// device, so punching across one is exactly the escape it would not
    /// catch, and a whole subtree excluded this way could be
    /// hiding an interrupted file recovery never looked at — precisely what
    /// `RecoveryReport::skipped`'s doc comment says a non-zero value means.
    OtherFilesystem,
}

/// Opens one name **from the directory descriptor it was listed in**, with
/// the flags its kind calls for, and then proves on the open descriptor what
/// it is.
///
/// `kind` is only the `d_type` from the listing: it decides which of the two
/// opens to attempt, and is never trusted for anything after that. A name
/// that was a regular file when it was listed and is a symlink by the time
/// it is opened is refused by `O_NOFOLLOW` (`ELOOP`); one that has become a
/// directory is refused by the `fstat` below; one that has become a FIFO or
/// a device node is opened once, read-only, and refused by the same `fstat`
/// before anything at all is read from it or written to it. (`O_NONBLOCK`
/// keeps a read-only open of a FIFO from waiting for a writer, and an
/// unprivileged process cannot create a device node inside a sync root to
/// begin with.)
///
/// Files are opened **read-only**: under the read
/// phase's lock every file is `0444`, and `O_RDWR` would refuse them all —
/// every interrupted file counted `skipped` and never reset. Nothing is
/// written through this descriptor; `reset_interrupted` reopens the one file
/// it is about to reset writable, on the same inode, so a file that is not
/// ours is never made writable, not even for the moment of an open.
fn open_entry(
    dir: &File,
    name: &OsString,
    kind: std::fs::FileType,
    root_dev: u64,
) -> io::Result<Entry> {
    let flags = if kind.is_dir() {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    } else if kind.is_file() {
        // Read-only: under the lock every file is
        // `0444`, and the one file recovery is about to reset is reopened
        // writable in `reset_interrupted` — never a file that is not ours.
        OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    } else {
        return Ok(Entry::Elsewhere);
    };
    let opened = match nix::fcntl::openat(dir.as_fd(), name.as_os_str(), flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        // The name stopped being what it was between the listing and here:
        // a symlink now (`ELOOP`), or no longer a directory (`ENOTDIR`, also
        // what `O_DIRECTORY | O_NOFOLLOW` reports for a symlink). Neither is
        // ours to open, and neither is a reason to stop.
        Err(Errno::ELOOP | Errno::ENOTDIR | Errno::EISDIR) => return Ok(Entry::Elsewhere),
        Err(e) => return Err(e.into()),
    };

    let meta = opened.metadata()?;
    if meta.dev() != root_dev {
        return Ok(Entry::OtherFilesystem);
    }
    if meta.is_dir() {
        return Ok(Entry::Directory(opened));
    }
    if !meta.is_file() {
        return Ok(Entry::Elsewhere);
    }
    let state = read_state(&opened);
    Ok(Entry::File(opened, state))
}

/// What to do about one open file: nothing at all unless it is one of ours
/// and the crash caught it mid-operation.
async fn recover_file(
    clearance: &Clearance,
    locks: &InodeLocks,
    file: File,
    state: Result<Option<State>, StateError>,
    shown: &Path,
    report: &mut RecoveryReport,
) {
    let state = match state {
        // Not a file this daemon manages. Not ours to count, and certainly
        // not ours to empty.
        Ok(None) => return,
        Ok(Some(state)) => state,
        // A managed file whose state we cannot make sense of, or cannot read
        // at all. Recovery must not punch it — `unwrap_or(None)` would have
        // called it "not one of ours" and moved on — but it must not pass
        // over it in silence either: §5.2 has the helper deny every open of
        // such a file with `EIO` for as long as it stays that way, and
        // recovery is the one place that walks the whole tree and could
        // notice.
        Err(e) => {
            report.skipped += 1;
            tracing::error!(
                "startup recovery: cannot read the state of {}: {e}. It is left exactly as it \
                 is — the helper denies every open of a managed file in an unknown state — but \
                 nothing here can repair it",
                shown.display()
            );
            return;
        }
    };
    report.scanned += 1;
    if !matches!(state, State::Hydrating | State::Dehydrating) {
        return;
    }
    // Not while this daemon fills or frees up the same file.
    let _guard = match InodeKey::of(&file) {
        Ok(key) => match locks.try_lock(key) {
            Some(guard) => guard,
            None => {
                report.busy += 1;
                tracing::info!(
                    "startup recovery: {} is {state:?} and being filled or freed up right now; \
                     left as it is",
                    shown.display()
                );
                return;
            }
        },
        Err(e) => {
            report.failed += 1;
            tracing::error!(
                "startup recovery: cannot tell which file {} is ({e}); left as found",
                shown.display()
            );
            return;
        }
    };
    match reset_interrupted(clearance, file).await {
        Ok(()) => report.reset += 1,
        Err(ResetError::Finished(now)) => {
            tracing::info!(
                "startup recovery: {} was {state:?} and is {now:?} now — finished while \
                 recovery looked at it; left as it is",
                shown.display()
            );
        }
        Err(ResetError::Unlinked) => {
            report.deferred += 1;
            tracing::info!(
                "startup recovery: {} is {state:?}, and a konedrive helper is running that this \
                 daemon is not connected to yet, so a mark it may hold on the file cannot be \
                 cleared; left as found until the connection is up",
                shown.display()
            );
        }
        Err(ResetError::InUse) => {
            report.busy += 1;
            tracing::info!(
                "startup recovery: {} is open elsewhere, {state:?}; left as found — its next \
                 open fills it, or the next start resets it",
                shown.display()
            );
        }
        Err(e) => {
            report.failed += 1;
            tracing::error!(
                "startup recovery: leaving {} exactly as found, {state:?}, for the next start: \
                 {e}",
                shown.display()
            );
        }
    }
}

/// Clears the ignore mark and punches one crash-interrupted file, both on
/// the inode the walk opened — through one writable reopen of the
/// descriptor the walk opened read-only (`/proc/self/fd/<n>`),
/// made before either — the identical sequence `dehydrate`'s
/// `mark_dehydrating`/`punch_clean_file` pair runs on a file this process is
/// actively working on, applied here to one a crash left mid-sequence
/// instead. Nothing here re-opens anything by path: the inode
/// that was classified is the inode that is punched, whatever the name points
/// at by the time the helper answers.
///
/// A `hydrating` file whose download left a checkpoint keeps the
/// checkpointed prefix and the checkpoint; only what lies past it
/// is punched, and the next open continues the download from there.
///
/// Returns before punching, leaving the file untouched, if `ClearIgnore` is
/// refused; that failure is never swallowed (see [`recover`]'s doc comment).
///
/// # The lease, and why recovery needs it more than `dehydrate`
///
/// `punch_clean_file` takes an `F_SETLEASE` before it empties a file, so
/// that an application which opens it mid-punch is suspended by the kernel
/// instead of reading a file with its blocks going away underneath. The
/// window is *wider* at startup, not narrower: the helper's own
/// `register_root` walk has to mark the whole tree before anything is
/// intercepted at all, so until that finishes any thumbnailer, backup or
/// indexer can have a `hydrating`/`dehydrating` file open while this runs.
/// A refused lease means exactly that — somebody has it open — so the file
/// is left as it was found and counted, for a start that finds it quieter.
///
/// # The mtime
///
/// `fallocate` moves the mtime to now, and §4.2 wants an `online-only`
/// file's mtime to be the remote `lastModifiedDateTime`. Recovery has no
/// remote metadata to restore, so it restores what the file had a moment
/// before the punch — which for a `dehydrating` file is exactly the remote
/// stamp `create_placeholder` set, and for a `hydrating` one is the time the
/// interrupted download last wrote, the best available answer until the next
/// hydration sets it properly. What it must not do is leave *now*: a whole
/// tree of files that recovery touched would then look locally modified, and
/// each is a hole full of zeros, which is an upload-over-remote hazard the
/// moment a delta engine exists. The stamp is removed in the same sequence,
/// so `dehydrate`'s "modified locally" guard is not what saves you.
///
/// In a folder that shows OneDrive, the time of a `hydrating` file kept with
/// its checkpoint is the one thing left wrong, and not for long: every
/// bring-up starts the folder's sync, whose first cycle is a Full reconcile,
/// and that puts the tree's time back without touching the checkpoint
/// (`materialize::check_file`).
async fn reset_interrupted(clearance: &Clearance, file: File) -> Result<(), ResetError> {
    // Writable only now, and only this file: the walk opened
    // every file read-only, and a file that is not ours is never made
    // writable, even for a moment — only a file the walk read `hydrating` or
    // `dehydrating` gets here. The reopen goes through the descriptor, so it
    // is the inode that was classified, whatever the name leads to by now
    //.
    //
    // It comes *before* the clear, and the read-only descriptor is closed at
    // once, so that the mark is cleared on, and the lease taken on, one and
    // the same open file. A write lease is refused while any other open file
    // of the inode exists, and the helper link sends a *duplicate* of the
    // descriptor it is given, which its writer thread drops only after the
    // send — possibly after the `Ack` has already brought this function to
    // its lease. With the read-only descriptor handed to the helper, that
    // duplicate kept the read-only open file alive and the lease was refused:
    // measured, every file `busy` and none reset with the writer thread
    // delayed 50 ms after its send. A duplicate of the descriptor the lease
    // is taken on is the same open file, and refuses nothing.
    let file = on_blocking_thread(move || {
        let writable = konedrive_fs::placeholder::reopen_writable(&file)?;
        drop(file);
        Ok::<_, io::Error>(writable)
    })
    .await??;
    // Invariant M3, on the very descriptor the punch will use — by the local
    // rule (see [`Clearance`]), for a root with interception or
    // without. It used to be skipped for a root without interception, on the
    // strength of a chain of reasoning about where a stale mark could be; the
    // chain was falsified three times, and nothing here
    // depends on it any more.
    clearance.clear(&file).await?;
    // `fault-injection` builds only: the VM suite's N3 scenario.
    fault::recovery_after_clear().await;
    on_blocking_thread(move || {
        let Some(lease) = lease_retrying_briefly(&file)? else {
            return Err(ResetError::InUse);
        };
        // Look again, now that nothing else has the file open.
        let state = match read_state(&file) {
            Ok(Some(state @ (State::Hydrating | State::Dehydrating))) => state,
            Ok(now) => return Err(ResetError::Finished(now)),
            Err(e) => return Err(ResetError::Io(io::Error::other(e.to_string()))),
        };
        let restore = FileTimes::of(&file)?;
        // A download's checkpoint is kept with its bytes. Only a
        // `hydrating` file can have one; the bytes it counts were made
        // durable before it was written, and the fill that continues from it
        // checks them against the quickXorHash along with the rest.
        let checkpoint = match state {
            State::Hydrating => read_progress(&file)
                .ok()
                .flatten()
                .filter(|p| p.bytes > 0 && p.bytes <= file.metadata().map(|m| m.len()).unwrap_or(0)),
            _ => None,
        };
        match checkpoint {
            Some(progress) => keep_checkpoint(&file, restore, progress.bytes)?,
            None => {
                // The attribute before the punch: a count of bytes must
                // never outlive the bytes it counts, not even across a
                // failure or a crash between the two.
                remove_progress(&file)?;
                punch_and_publish(&file, restore)?;
            }
        }
        drop(lease);
        Ok(())
    })
    .await?
}

/// The write lease recovery punches under, with a refusal retried a few
/// times over about 75 ms before the file counts as in use.
///
/// `F_SETLEASE` is refused while any other open file of the inode exists
/// anywhere, and recovery's walk makes one of its own it cannot fully control:
/// the read-only descriptor it classified the file on, closed before the
/// lease — but a process being spawned at that moment holds an inherited copy
/// of it until its `exec`, and that copy alone refuses the lease. Measured on
/// this module's tests, where one test spawns `unshare`: 2 runs in 10 left an
/// interrupted file `busy`, none in 20 with that test skipped, and none in 30
/// on part 1's code, whose walk and lease shared one open file. Such a copy
/// goes within milliseconds; an application holding the file open does not,
/// and still finds the file `busy` after the last try, as before.
fn lease_retrying_briefly(file: &File) -> io::Result<Option<WriteLease<'_>>> {
    for pause in [5, 20, 50] {
        if let Some(lease) = WriteLease::take(file)? {
            return Ok(Some(lease));
        }
        std::thread::sleep(std::time::Duration::from_millis(pause));
    }
    WriteLease::take(file)
}

/// [`punch_and_publish`] for an interrupted download with a checkpoint: only
/// what lies past the checkpoint is punched, and the checkpoint stays
///. The order is otherwise the same — the punch and the mtime
/// made durable before the file is called `online-only`.
fn keep_checkpoint(file: &File, restore: FileTimes, bytes: u64) -> io::Result<()> {
    punch_from(file, bytes)?;
    restore.restore(file)?;
    file.sync_all()?;
    write_state(file, State::OnlineOnly)?;
    remove_stamp(file)
}

/// A deliberate stall for a race window too narrow to hit by chance —
/// compiled in **only** with the `fault-injection` cargo feature, like the
/// helper's. `tests/vm/Cargo.toml` builds this crate with it
/// for the VM suite; the daemon that ships does not have it.
///
/// Startup recovery clears a file's ignore mark and then takes its write
/// lease. A fill from the previous
/// helper connection can commit `hydrated` in between, and an opener can have
/// the file ignore-marked, in well under a millisecond; the VM suite widens
/// that gap to put both inside it.
#[cfg(feature = "fault-injection")]
pub mod fault {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static RECOVERY_STALL_MS: AtomicU64 = AtomicU64::new(0);

    /// From now on, every file recovery is about to reset waits this long
    /// between clearing its ignore mark and taking its lease. Zero disarms.
    pub fn set_recovery_stall(stall: Duration) {
        RECOVERY_STALL_MS.store(stall.as_millis() as u64, Ordering::SeqCst);
    }

    pub(super) async fn recovery_after_clear() {
        let ms = RECOVERY_STALL_MS.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

/// The shipped build: nothing to arm, nothing compiled in.
#[cfg(not(feature = "fault-injection"))]
mod fault {
    #[inline(always)]
    pub(super) async fn recovery_after_clear() {}
}

/// Runs one blocking step of the walk on a blocking thread:
/// `openat`, `getxattr`, `fallocate` and `fsync` are all blocking syscalls,
/// and `sync/helper.rs`'s module doc treats a blocking call left on a tokio
/// worker as a first-class defect.
async fn on_blocking_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(format!("the recovery task failed: {e}")))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use konedrive_fs::placeholder::{
        read_progress, read_stamp, read_state, write_progress, write_stamp, Progress, State,
    };
    use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
    use nix::sys::socket::{
        accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
        UnixAddr,
    };

    use super::*;

    /// A sync root the way `register_root` would have left one: resolved
    /// path, root id on the folder.
    fn test_root(dir: &Path) -> SyncRoot {
        let path = dir.canonicalize().unwrap();
        let root_id = uuid_v4();
        let handle = File::open(&path).unwrap();
        handle.set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
        SyncRoot { path, root_id }
    }

    /// A hydrated file of `size` bytes with a matching stamp, exactly as a
    /// finished hydration leaves one.
    fn hydrated_file(root: &Path, name: &str, size: usize) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, vec![1u8; size]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        write_state(&file, State::Hydrated).unwrap();
        write_stamp(&file).unwrap();
        path
    }

    fn open_rw(path: &Path) -> File {
        File::options().read(true).write(true).open(path).unwrap()
    }

    /// The recovery every test in this module runs: through a link to its
    /// fake helper, which is what an intercepted root recovers through.
    /// Shadows `super::recover` on purpose, so the tests read as they did
    /// before recovery took a [`Clearance`].
    async fn recover(link: &HelperLink, root: &SyncRoot) -> Result<RecoveryReport, RecoveryError> {
        super::recover(&Clearance::Link(link.clone()), root, &InodeLocks::new()).await
    }

    fn blocks_of(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().blocks()
    }

    // --- The local guard -------------------------------------------------

    #[tokio::test]
    async fn refuses_a_non_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stray.txt"), b"x").unwrap();
        let error = check_root_candidate(dir.path()).unwrap_err();
        assert!(matches!(error, RegisterError::NotEmpty), "{error:?}");
    }

    #[tokio::test]
    async fn accepts_an_empty_directory_on_a_supported_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        check_root_candidate(dir.path()).unwrap();
    }

    /// A file, a missing path and a symlink are all "not a folder you can
    /// register", and each has to say so as itself rather than as whatever
    /// the next syscall along happens to complain about.
    #[tokio::test]
    async fn refuses_anything_that_is_not_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        assert!(
            matches!(check_root_candidate(&file).unwrap_err(), RegisterError::NotADirectory),
            "a plain file must be refused as not a directory"
        );
        assert!(
            matches!(
                check_root_candidate(&dir.path().join("nope")).unwrap_err(),
                RegisterError::NotADirectory
            ),
            "a path that does not exist must be refused as not a directory"
        );

        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = check_root_candidate(&link).unwrap_err();
        assert!(
            matches!(&error, RegisterError::Unsupported(why) if why.contains("symbolic link")),
            "{error:?}"
        );
    }

    /// The probe is not decoration: a directory can be empty, be a
    /// directory, and still be unable to hold a single placeholder. Here it
    /// is one we cannot write into at all — the cheapest unprivileged stand-in
    /// for a filesystem that refuses the features, and the one thing that
    /// fails if `probe_dir` is dropped from the checks.
    #[tokio::test]
    async fn refuses_a_directory_the_probe_cannot_use() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

        let error = check_root_candidate(&readonly).unwrap_err();

        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(&error, RegisterError::Unsupported(why) if why.contains(&readonly.display().to_string())),
            "{error:?}"
        );
    }

    /// The empty requirement belongs to *first* registration
    /// only. A folder that already carries a valid `user.konedrive.root`
    /// is a root being re-registered, and re-registration is expected to
    /// find it full of exactly the placeholders and hydrated files this
    /// daemon itself put there — refusing it as though it were some other,
    /// foreign non-empty folder would make every restart unregister every
    /// root.
    ///. H78 waives the empty check for a folder that "already
    /// carries a root id", and nothing ever removes that xattr again — so if
    /// any string counts, one `setfattr -n user.konedrive.root -v x` makes a
    /// folder full of somebody's existing documents registerable, for good.
    /// Only the id form this daemon actually mints may waive it.
    #[tokio::test]
    async fn a_value_that_is_not_one_of_our_root_ids_does_not_waive_the_empty_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("their-thesis.odt"), b"x").unwrap();
        let handle = File::open(dir.path()).unwrap();

        for bogus in [
            "",                                     // present but empty
            "x",                                    // the one-character setfattr
            "not-a-uuid",                           // a word
            "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5",  // 35 characters
            "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d6", // 37
            "1c2e4f5a-0b3c-3d5e-8f60-71829a3b4c5d", // version 3, not 4
            "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4czd", // not hex
            "1c2e4f5a0b3c4d5e8f6071829a3b4c5d6e7f", // 36 characters, no dashes
        ] {
            handle.set_xattr(XATTR_ROOT, bogus.as_bytes()).unwrap();
            let error = check_root_candidate(dir.path()).unwrap_err();
            assert!(
                matches!(error, RegisterError::NotEmpty),
                "{bogus:?} waived the empty check: {error:?}"
            );
        }

        handle.set_xattr(XATTR_ROOT, uuid_v4().as_bytes()).unwrap();
        check_root_candidate(dir.path()).expect("a real root id must still waive it");
    }

    /// The other half of H90: a value that is not an id of ours names no
    /// registration the helper could be holding, so "never
    /// overwrite an existing id" does not apply to it — a real one is minted
    /// over the top rather than the junk being offered to the helper as this
    /// root's name.
    #[tokio::test]
    async fn a_bogus_root_id_is_replaced_by_a_real_one() {
        let dir = tempfile::tempdir().unwrap();
        File::open(dir.path()).unwrap().set_xattr(XATTR_ROOT, b"x").unwrap();

        let (_dir, root) = prepare_root(dir.path()).unwrap();

        // Spelled out rather than asked of `looks_like_a_root_id`, which is
        // the function under test: it would agree with itself.
        assert_ne!(root.root_id, "x", "the junk value was offered to the helper as this root");
        assert_eq!(root.root_id.len(), 36, "{}", root.root_id);
        let fields: Vec<&str> = root.root_id.split('-').collect();
        assert_eq!(fields.iter().map(|f| f.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(fields[2].starts_with('4'), "{}", root.root_id);
        assert_eq!(
            File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap().as_deref(),
            Some(root.root_id.as_bytes())
        );
    }

    /// The probe answers first. `read_root_id` is a `getxattr` in
    /// the `user.*` namespace — the very thing the probe exists to establish
    /// is available — so asking it first replaces the probe's purpose-built
    /// message with an errno about an attribute name, on a folder it does not
    /// name. Here the folder is both unusable and non-empty, and it is the
    /// unusability that must be reported: a folder that cannot hold a
    /// placeholder at all cannot be a sync root whether it is empty or not.
    #[tokio::test]
    async fn an_unusable_directory_is_reported_as_unusable_even_when_it_is_not_empty() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly).unwrap();
        std::fs::write(readonly.join("stray.txt"), b"x").unwrap();
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();

        let error = check_root_candidate(&readonly).unwrap_err();

        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(&error, RegisterError::Unsupported(why) if why.contains(&readonly.display().to_string())),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn accepts_a_non_empty_directory_that_already_carries_its_own_root_id() {
        let dir = tempfile::tempdir().unwrap();
        let handle = File::open(dir.path()).unwrap();
        handle.set_xattr(XATTR_ROOT, uuid_v4().as_bytes()).unwrap();
        std::fs::write(dir.path().join("stray.txt"), b"x").unwrap();

        check_root_candidate(dir.path()).unwrap();
    }

    // --- Root registration -----------------------------------------------

    #[tokio::test]
    async fn register_root_stamps_the_folder_and_tells_the_helper() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let root = register_root(&link, dir.path()).await.unwrap();

        assert_eq!(root.path, dir.path().canonicalize().unwrap());
        let on_disk = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap();
        assert_eq!(
            on_disk.as_deref(),
            Some(root.root_id.as_bytes()),
            "the folder must carry the id the helper was told about"
        );
        asked_to(&helper, "RegisterRoot");
    }

    /// The first attempt fails after the folder has been
    /// stamped; the second must offer the helper the *same* id, because the
    /// helper may already be holding it — a fresh one is refused as a
    /// conflicting registration of the same directory, for good.
    #[tokio::test]
    async fn register_root_reuses_the_id_already_on_the_folder() {
        let dir = tempfile::tempdir().unwrap();

        let refusing = tempfile::tempdir().unwrap();
        let refusing_socket = refusing.path().join("helper.sock");
        let _refusing = fake_helper_refusing_everything(refusing_socket.clone());
        let (refused_link, _r) = HelperLink::connect(&refusing_socket).await.unwrap();
        let error = register_root(&refused_link, dir.path()).await.unwrap_err();
        assert!(matches!(error, RegisterError::Helper(_)), "{error:?}");

        let first = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap().unwrap();

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let root = register_root(&link, dir.path()).await.unwrap();

        assert_eq!(
            root.root_id.as_bytes(),
            first.as_slice(),
            "the retry minted a new id; the helper would refuse it as a second registration of \
             the same directory, EINVAL, forever"
        );
        let on_disk = File::open(dir.path()).unwrap().get_xattr(XATTR_ROOT).unwrap();
        assert_eq!(on_disk.as_deref(), Some(first.as_slice()));
    }

    /// The actual startup scenario: the daemon registers a
    /// folder, populates it (placeholders, hydrated files — anything, here
    /// just a plain file stands in), then restarts and registers the same
    /// folder again. The second call must not be refused `NotEmpty`.
    #[tokio::test]
    async fn register_root_accepts_a_populated_folder_on_a_second_registration() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let first = register_root(&link, dir.path()).await.unwrap();

        // Stand in for what a real run would have left behind.
        std::fs::write(dir.path().join("placeholder.bin"), vec![1u8; 4096]).unwrap();

        let second = register_root(&link, dir.path()).await.unwrap();
        assert_eq!(
            second.root_id, first.root_id,
            "re-registration must not mint a new id (Ruling H70)"
        );
    }

    #[test]
    fn uuid_v4_mints_a_fresh_identifier_every_time() {
        let minted: std::collections::HashSet<String> = (0..64).map(|_| uuid_v4()).collect();
        assert_eq!(minted.len(), 64, "root ids must be unique: two folders must never collide");
        for id in &minted {
            assert_eq!(id.len(), 36, "{id}");
            let fields: Vec<&str> = id.split('-').collect();
            assert_eq!(fields.iter().map(|f| f.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
            assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{id}");
            assert!(fields[2].starts_with('4'), "version 4 expected: {id}");
            assert!(matches!(&fields[3][0..1], "8" | "9" | "a" | "b"), "variant expected: {id}");
        }
    }

    // --- The dehydration guard -------------------------------------------

    #[tokio::test]
    async fn dehydration_refuses_a_locally_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = hydrated_file(dir.path(), "f.bin", 4096);

        let mut appended = File::options().append(true).open(&path).unwrap();
        appended.write_all(b"changed").unwrap();
        drop(appended);

        let error = check_dehydratable(&open_rw(&path)).unwrap_err();
        assert!(matches!(error, DehydrateError::ModifiedLocally), "{error:?}");
    }

    /// The state gate, arm by arm. Only `hydrated` may be emptied: a file
    /// that is `online-only` has nothing to free, and one that is `hydrating`
    /// or `dehydrating` is in the middle of something — punching any of them
    /// on the strength of a stale stamp is how a download in flight becomes
    /// zeros.
    #[tokio::test]
    async fn dehydration_refuses_every_state_but_hydrated() {
        let dir = tempfile::tempdir().unwrap();
        for state in [State::OnlineOnly, State::Hydrating, State::Dehydrating] {
            let path = hydrated_file(dir.path(), &format!("{}.bin", state.as_str()), 4096);
            let file = open_rw(&path);
            write_state(&file, state).unwrap();

            let error = check_dehydratable(&file).unwrap_err();
            assert!(matches!(error, DehydrateError::NotHydrated), "{state:?}: {error:?}");
        }
    }

    /// A file with no `user.konedrive.state` at all is not ours. Nothing
    /// about it — not its name, not where it sits — makes it something this
    /// daemon may empty.
    #[tokio::test]
    async fn dehydration_refuses_a_file_that_is_not_managed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.txt");
        std::fs::write(&path, vec![9u8; 4096]).unwrap();

        let error = check_dehydratable(&open_rw(&path)).unwrap_err();
        assert!(matches!(error, DehydrateError::NotManaged), "{error:?}");
    }

    #[tokio::test]
    async fn dehydration_empties_a_clean_file_and_keeps_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = hydrated_file(dir.path(), "f.bin", 1 << 20);

        let file = open_rw(&path);
        let restore = mark_dehydrating(&file).unwrap();
        punch_clean_file(&file, restore).unwrap();
        drop(file);

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1 << 20);
        assert!(meta.blocks() < 64, "{} blocks left", meta.blocks());
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(
            read_stamp(&file).unwrap(),
            None,
            "the stamp described a hydrated file and must not outlive it"
        );
    }

    /// An `online-only` file's mtime is the remote
    /// `lastModifiedDateTime`. `fallocate` bumps it to now, so dehydration
    /// has to put it back — otherwise "free up space" silently makes every
    /// file look modified today, which is precisely the signal the Sync
    /// engine's change detection will key on.
    #[tokio::test]
    async fn dehydration_keeps_the_remote_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, vec![1u8; 1 << 20]).unwrap();
        let remote = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let file = open_rw(&path);
        file.set_times(std::fs::FileTimes::new().set_modified(remote)).unwrap();
        write_state(&file, State::Hydrated).unwrap();
        write_stamp(&file).unwrap();

        let restore = mark_dehydrating(&file).unwrap();
        punch_clean_file(&file, restore).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            remote,
            "dehydration must not move the file's mtime to now"
        );
    }

    /// The lease is the proof that nobody else has the file open, so a
    /// refusal has to stop everything — and put the state back, or the file
    /// is left claiming to be mid-dehydration when nothing is happening to
    /// it at all.
    #[tokio::test]
    async fn dehydration_refuses_while_another_process_holds_the_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = hydrated_file(dir.path(), "f.bin", 4096);

        let file = open_rw(&path);
        let restore = mark_dehydrating(&file).unwrap();
        let _held_open = File::open(&path).unwrap();

        let error = punch_clean_file(&file, restore).unwrap_err();
        assert!(matches!(error, DehydrateError::InUse), "{error:?}");
        assert_eq!(
            read_state(&file).unwrap(),
            Some(State::Hydrated),
            "a refused lease must roll the state back, not leave the file dehydrating"
        );
        assert!(blocks_of(&path) > 0, "nothing may be punched without the lease");
    }

    /// The lease has to still be held at the moment the blocks go
    /// away *and* until the file has been published `online-only` — an open
    /// that slips in after an early release is not suspended, and reads a
    /// file being emptied under it.
    ///
    /// The version this replaces took `fcntl(F_GETLEASE)` from the hook and
    /// asserted it said `F_WRLCK`. That measures "a lease existed when the
    /// hook ran", which is not what its name claimed: releasing the lease
    /// *between the hook and the punch*, or *between the punch and the state
    /// flip*, both survived it. Measured — and the shipped code was correct,
    /// so the test was the thing that was wrong.
    ///
    /// So the lease is measured by what it does, at both of its ends: a
    /// thread started at `UnderLease` opens the file, and it must **still be
    /// suspended** at `BeforeRelease`, with the punch and the state flip both
    /// behind it. Each end waits long enough (200 ms) that an early release
    /// would have let the opener through many times over, so neither
    /// assertion is a race in either direction — a correctly held lease
    /// cannot let it through at all, and a released one needs microseconds.
    /// What the opener finally sees when it does get through is checked too.
    /// `konedrive-fs`'s `a_lease_break_does_not_kill_the_process` has the
    /// same shape.
    #[test]
    fn an_open_arriving_during_the_punch_waits_for_all_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = hydrated_file(dir.path(), "f.bin", 1 << 20);

        let file = open_rw(&path);
        let restore = mark_dehydrating(&file).unwrap();

        // The opener's own two signals: it is about to call `open()`, and it
        // has come back from it.
        let entering = Arc::new(AtomicBool::new(false));
        let through = Arc::new(AtomicBool::new(false));
        let (openers, opener) = std::sync::mpsc::channel();

        let hook = {
            let path = path.clone();
            let (entering, through) = (Arc::clone(&entering), Arc::clone(&through));
            let mut path = Some(path);
            move |watch: Watch, _: &File| {
                if let Some(path) = path.take() {
                    let (signals, watched) = (Arc::clone(&entering), Arc::clone(&through));
                    openers
                        .send(std::thread::spawn(move || {
                            signals.store(true, Ordering::SeqCst);
                            let opened = File::open(&path).unwrap();
                            watched.store(true, Ordering::SeqCst);
                            // What the application would see, sampled the
                            // instant its `open()` succeeded.
                            let meta = opened.metadata().unwrap();
                            (read_state(&opened).unwrap(), meta.blocks())
                        }))
                        .unwrap();
                    while !entering.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
                assert!(
                    !through.load(Ordering::SeqCst),
                    "{watch:?}: the open went through while the file was being emptied — the \
                     lease was not held across all of the punch and the publish, so an \
                     application read a file mid-dehydration"
                );
            }
        };
        punch_clean_file_watched(&file, restore, hook).unwrap();

        let (state, blocks) = opener.recv().unwrap().join().unwrap();
        assert_eq!(
            state,
            Some(State::OnlineOnly),
            "the open completed while the file was still dehydrating: the lease did not cover \
             the whole sequence"
        );
        assert!(blocks < 64, "the open completed while the file still had its blocks");
    }

    /// Makes `fsync`/`fdatasync` fail with `EIO` for this process, for good,
    /// so that a missing durability barrier becomes an observable difference
    /// rather than an invisible one.
    ///
    /// A seccomp filter is the only way to do that unprivileged: nothing in
    /// user space can make tmpfs refuse an `fsync`, and the crash injection
    /// that would show the barrier's real purpose needs the VM suite (spec
    /// §11.2). The filter applies to the calling thread only (no `TSYNC`)
    /// and cannot be lifted, so the one test that uses it does so on a
    /// thread it is willing to lose.
    ///
    /// `Err` means seccomp is not available here (an old kernel, a sandbox
    /// that blocks it); the caller then skips rather than fails.
    fn deny_fsync() -> Result<(), ()> {
        deny_syscalls(&[libc::SYS_fsync, libc::SYS_fdatasync])
    }

    /// The general form: make each of `numbers` fail with `EIO` for this
    /// thread, for good. Used for `fsync`/`fdatasync` (a missing durability
    /// barrier) and for `fallocate` (a punch that fails), neither of which
    /// an unprivileged test can provoke any other way on tmpfs.
    fn deny_syscalls(numbers: &[libc::c_long]) -> Result<(), ()> {
        // `no_new_privs` is a per-thread, inherited flag; it is what lets an
        // unprivileged thread install a filter at all.
        const LOAD_SYSCALL_NR: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
        const JUMP_IF_EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
        const RETURN: u16 = 0x06; // BPF_RET | BPF_K
        const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
        const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
        const SECCOMP_MODE_FILTER: i32 = 2;

        // nr; one `jeq <number> -> deny` per number; allow; deny. Each jump
        // is taken to the last instruction, so `jt` counts the instructions
        // between it and the end.
        let mut program = vec![libc::sock_filter { code: LOAD_SYSCALL_NR, jt: 0, jf: 0, k: 0 }];
        for (i, number) in numbers.iter().enumerate() {
            program.push(libc::sock_filter {
                code: JUMP_IF_EQUAL,
                jt: (numbers.len() - i) as u8,
                jf: 0,
                k: *number as u32,
            });
        }
        program.push(libc::sock_filter { code: RETURN, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW });
        program.push(libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EIO as u32 & 0xffff),
        });
        let filter = libc::sock_fprog {
            len: program.len() as u16,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: both prctls take a live, correctly sized `sock_fprog`
        // whose filter array outlives the call.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(());
            }
            if libc::prctl(libc::PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &filter) != 0 {
                return Err(());
            }
        }
        Ok(())
    }

    /// Runs an async body with `denied` failing on **every** thread it can
    /// reach: the thread the future runs on, and every blocking thread
    /// `spawn_blocking` hands work to (`on_thread_start` covers the blocking
    /// pool as well as the runtime's own threads — `tokio`'s
    /// `blocking::pool::Inner::run` calls `after_start`). Without that, a
    /// filter installed on the test's thread would not reach the phase under
    /// test, which is always on a blocking thread.
    ///
    /// The whole runtime lives on one thread of its own, which is then
    /// dropped, because a seccomp filter cannot be lifted. Anything the test
    /// needs *unfiltered* — the fake helper — must be started before this is
    /// called, since threads created from inside inherit the filter.
    ///
    /// `None` means seccomp is unavailable here (an old kernel, a sandbox
    /// that blocks it); the caller then skips rather than fails.
    fn with_syscalls_denied<T, F>(
        denied: &'static [libc::c_long],
        body: impl FnOnce() -> F + Send + 'static,
    ) -> Option<T>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T>,
    {
        std::thread::spawn(move || {
            deny_syscalls(denied).ok()?;
            let unfiltered = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&unfiltered);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .on_thread_start(move || {
                    if deny_syscalls(denied).is_err() {
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                })
                .build()
                .unwrap();
            let outcome = runtime.block_on(body());
            (unfiltered.load(Ordering::SeqCst) == 0).then_some(outcome)
        })
        .join()
        .expect("the thread running the filtered work must not panic")
    }

    /// Dehydration's step 4: the punch is followed by an `fsync`, and the file is
    /// not called `online-only` until that has succeeded. A punch that is
    /// only in page cache, published as `online-only`, is a file the next
    /// boot can find with its blocks back and its state insisting they are
    /// gone — and nothing will hydrate it, because `online-only` is exactly
    /// the state that means "the content is elsewhere".
    ///
    /// A barrier that works is invisible, so it is measured by taking it
    /// away. The file is marked `dehydrating` first, while `fsync` still
    /// works, so the only barrier the filter can remove is the one that
    /// follows the punch — the thing under test.
    ///
    /// The work runs on a thread of its own because a seccomp filter cannot
    /// be lifted: letting it die with the thread keeps it away from every
    /// other test, including under `--test-threads=1`, where libtest runs
    /// the test bodies themselves on the main thread. A thread, not a child
    /// process: an earlier version of this test forked, and the other
    /// lease-taking tests in this binary then failed roughly one run in five.
    /// **Why** was never established. This comment used to say that spawning
    /// duplicates the descriptor table and that a duplicated descriptor is
    /// what `F_SETLEASE` refuses on; that is false — measured on this kernel,
    /// 0 failures in 2000 `posix_spawn`s and 0 in 3000 `fork`s either side of
    /// an `exec`, against a control where a real second `open()` gives
    /// `EAGAIN` immediately, because the check reads
    /// `inode->i_readcount`/`i_writecount`, which only a genuine open raises.
    /// The flakiness was real, its cause is unknown, and the thread stays.
    #[test]
    fn the_punch_is_made_durable_before_the_file_is_called_online_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = hydrated_file(dir.path(), "f.bin", 1 << 20);
        let file = open_rw(&path);
        let restore = mark_dehydrating(&file).unwrap();

        let punched = std::thread::spawn(move || match deny_fsync() {
            Err(()) => None,
            Ok(()) => Some(punch_clean_file(&file, restore).is_ok()),
        })
        .join()
        .expect("the thread running the punch must not panic");

        match punched {
            None => eprintln!("seccomp is unavailable here; skipping the durability check"),
            Some(true) => panic!(
                "the dehydration reported success although every fsync failed: the punch is \
                 never made durable"
            ),
            Some(false) => {
                assert!(blocks_of(&path) < 64, "the punch itself should still have happened");
                assert_eq!(
                    read_state(&File::open(&path).unwrap()).unwrap(),
                    Some(State::Dehydrating),
                    "a file whose punch could not be made durable must not be published as \
                     online-only"
                );
            }
        }
    }

    /// Step 2: `state=dehydrating` is made durable
    /// **before** the helper is asked to stop intercepting the file. Without
    /// that barrier a crash in the window can leave the punch durable while
    /// the state is not — an empty file that reads `hydrated` with its ignore
    /// mark cleared, which the helper then allows and re-marks: zeros on
    /// every later open, which is the one outcome this sub-project exists to
    /// prevent.
    ///
    /// `the_punch_is_made_durable_before_the_file_is_called_online_only`
    /// cannot reach this one: it installs its filter *after* marking, so the
    /// only barrier it can take away is the one after the punch — and
    /// deleting this `fsync` broke nothing. Here the filter is in place
    /// before `mark_dehydrating` runs, and the assertion is that the helper
    /// is never asked anything at all.
    #[test]
    fn the_dehydrating_state_is_durable_before_the_helper_is_asked_to_stop_intercepting() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = hydrated_file(&root.path, "f.bin", 1 << 20);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        // Started before the filter exists, on a thread that never inherits
        // it: the helper must be able to answer, so that "it was never asked"
        // is a real observation rather than an artefact.
        let helper = fake_helper(socket_path.clone(), 0, || {});

        let asked = root.clone();
        let target = path.clone();
        let denied = &[libc::SYS_fsync, libc::SYS_fdatasync];
        let outcome = with_syscalls_denied(denied, move || async move {
            let link = connected(&socket_path).await;
            dehydrate(&link, &asked, &target).await
        });

        let Some(outcome) = outcome else {
            eprintln!("seccomp is unavailable here; skipping the durability check");
            return;
        };
        assert!(
            outcome.is_err(),
            "a dehydration whose state could not be made durable reported success"
        );
        assert!(
            helper.recv_timeout(Duration::from_millis(200)).is_err(),
            "the helper was asked to clear the ignore mark although `dehydrating` was never made \
             durable: a crash in that window leaves an empty file reading `hydrated`"
        );
        // And the failure of the barrier itself rolls back, so the
        // file is not left announcing a dehydration that never started.
        assert_eq!(
            read_state(&File::open(&path).unwrap()).unwrap(),
            Some(State::Hydrated),
            "a file whose `dehydrating` could not be made durable must not be left claiming it: \
             startup recovery would empty a fully hydrated file"
        );
        assert!(blocks_of(&path) > 64, "nothing may be punched");
    }

    // --- The helper round trip -------------------------------------------

    /// What a fake helper saw: the message, and what the descriptor it came
    /// with looked like at that moment.
    #[derive(Debug)]
    struct Seen {
        message: String,
        state: Option<State>,
        ino: u64,
    }

    /// A stand-in helper: greets, acks the `Hello` handshake, then answers
    /// `ClearIgnore` with `clear_ignore_errno` and everything else with 0.
    ///
    /// `on_clear_ignore` runs on the helper's thread while the daemon is
    /// still awaiting the ack — the one moment in `dehydrate` when the file
    /// is marked, the helper has the descriptor, and nothing has been punched
    /// yet. That makes it the injection point for everything that used to be
    /// a race.
    ///
    /// The listener is bound on the caller's thread before this returns, so
    /// the test's own `connect` cannot race `bind`.
    fn fake_helper(
        path: PathBuf,
        clear_ignore_errno: i32,
        on_clear_ignore: impl FnOnce() + Send + 'static,
    ) -> std::sync::mpsc::Receiver<Seen> {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        bind(fd.as_raw_fd(), &addr).unwrap();
        sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let listener = fd;
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: `accept` just returned a freshly opened descriptor that
            // this process now solely owns.
            let stream = unsafe { UnixStream::from_raw_fd(accepted) };
            let mut channel = Channel::new(stream).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let (hello, _) = channel.recv::<ToHelper>().unwrap();
            assert!(
                matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION),
                "{hello:?}"
            );
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();

            let mut hook = Some(on_clear_ignore);
            while let Ok((message, fd)) = channel.recv::<ToHelper>() {
                let clearing = matches!(message, ToHelper::ClearIgnore);
                let (state, ino) = match fd {
                    Some(fd) => {
                        let file = File::from(fd);
                        (
                            read_state(&file).ok().flatten(),
                            file.metadata().map(|m| m.ino()).unwrap_or(0),
                        )
                    }
                    None => (None, 0),
                };
                let _ = tx.send(Seen { message: format!("{message:?}"), state, ino });
                if clearing {
                    if let Some(hook) = hook.take() {
                        hook();
                    }
                }
                let errno = if clearing { clear_ignore_errno } else { 0 };
                if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                    break;
                }
            }
        });
        rx
    }

    /// The same, but every request after the handshake is refused. Used to
    /// fail a registration the way a real helper would.
    fn fake_helper_refusing_everything(path: PathBuf) -> std::sync::mpsc::Receiver<Seen> {
        fake_helper_with_errno(path, libc::EPERM)
    }

    fn fake_helper_with_errno(path: PathBuf, errno: i32) -> std::sync::mpsc::Receiver<Seen> {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        bind(fd.as_raw_fd(), &addr).unwrap();
        sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let listener = fd;
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: as above — a descriptor `accept` just handed us.
            let stream = unsafe { UnixStream::from_raw_fd(accepted) };
            let mut channel = Channel::new(stream).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let (_hello, _) = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
                let _ = tx.send(Seen { message: format!("{message:?}"), state: None, ino: 0 });
                if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                    break;
                }
            }
        });
        rx
    }

    async fn connected(socket_path: &Path) -> HelperLink {
        HelperLink::connect(socket_path).await.unwrap().0
    }

    /// The first request of a given kind the fake helper saw, or a failure
    /// if it never arrived. Bounded, so a mutation that stops calling the
    /// helper at all fails the test instead of hanging it.
    fn asked_to(seen: &std::sync::mpsc::Receiver<Seen>, what: &str) -> Seen {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while let Ok(request) =
            seen.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            if request.message.starts_with(what) {
                return request;
            }
        }
        panic!("the helper was never asked to {what}");
    }

    /// Invariant M3, and the one failure in this module that is both silent
    /// and unrecoverable: if the ignore mark was not cleared, the file must
    /// come out of this untouched. A `let _ = link.clear_ignore(...)` here
    /// punches anyway and leaves a file that is empty *and* invisible to the
    /// helper — zeros on every later open, forever.
    #[tokio::test]
    async fn dehydrate_leaves_the_file_untouched_when_clear_ignore_fails() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), libc::EIO, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = hydrated_file(&root.path, "f.bin", 1 << 20);

        let error = dehydrate(&link, &root, &path).await.unwrap_err();
        assert!(matches!(error, DehydrateError::Io(_)), "{error:?}");

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1 << 20, "size must be untouched");
        assert!(meta.blocks() > 64, "the file must NOT be punched when ClearIgnore failed");
        let file = File::open(&path).unwrap();
        assert_eq!(
            read_state(&file).unwrap(),
            Some(State::Hydrated),
            "a failed ClearIgnore must roll the state back, not leave the file dehydrating"
        );
    }

    /// / steps 1–2, in that order. By the time the helper
    /// is asked to stop intercepting this file, the file must already say
    /// `dehydrating` — otherwise an open landing in the gap is answered with
    /// a *fresh* ignore mark and an allow, and the punch that follows leaves
    /// the file empty and permanently un-intercepted, with no error anywhere.
    /// The state is read off the very descriptor the helper was handed.
    #[tokio::test]
    async fn dehydrate_marks_the_file_dehydrating_before_it_asks_the_helper() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = hydrated_file(&root.path, "f.bin", 1 << 20);
        let expected_ino = std::fs::metadata(&path).unwrap().ino();

        dehydrate(&link, &root, &path).await.unwrap();

        let seen = asked_to(&helper, "ClearIgnore");
        assert_eq!(
            seen.state,
            Some(State::Dehydrating),
            "the helper saw state={:?}: the mark is being cleared while the file still reads \
             hydrated, so an open in that window gets a fresh ignore mark and an allow",
            seen.state
        );
        assert_eq!(seen.ino, expected_ino, "the helper was handed a different file");
    }

    /// The defect that destroyed 300 KiB of real data three runs
    /// out of three. The file is replaced — an editor's save-and-replace, a
    /// `mv`, anything — at the one moment the daemon is not holding still:
    /// while it waits for the helper's `ClearIgnore` ack. A by-path reopen
    /// after that point punches the replacement, which was never checked,
    /// never carried a konedrive xattr, and is somebody's fresh work. With a
    /// single descriptor there is nothing to re-resolve: the punch lands on
    /// the inode the guard passed, whatever the name now points at.
    #[tokio::test]
    async fn dehydrate_punches_the_file_it_checked_even_if_the_name_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = hydrated_file(&root.path, "f.bin", 300 * 1024);
        // A second name for the same inode, so the original stays reachable
        // after the swap. A hard link is not an open descriptor, so it does
        // not disturb the write lease.
        let original = root.path.join("original.link");
        std::fs::hard_link(&path, &original).unwrap();

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let swap_in = root.path.join("replacement.tmp");
        std::fs::write(&swap_in, vec![0xABu8; 300 * 1024]).unwrap();
        let swapped_to = path.clone();
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            std::fs::rename(&swap_in, &swapped_to).unwrap();
        });
        let link = connected(&socket_path).await;

        dehydrate(&link, &root, &path).await.unwrap();

        let replacement = std::fs::read(&path).unwrap();
        assert!(
            replacement.iter().all(|b| *b == 0xAB),
            "the replacement file was punched: {} of its {} bytes are zero",
            replacement.iter().filter(|b| **b == 0).count(),
            replacement.len()
        );
        assert!(blocks_of(&path) > 64, "the replacement file lost its blocks");
        assert_eq!(
            File::open(&path).unwrap().get_xattr("user.konedrive.state").unwrap(),
            None,
            "a file konedrive never managed was stamped by the dehydration"
        );

        assert!(blocks_of(&original) < 64, "the file that was checked was not punched");
        assert_eq!(
            read_state(&File::open(&original).unwrap()).unwrap(),
            Some(State::OnlineOnly)
        );
    }

    /// The guard runs before the helper is involved at all: a file that may
    /// not be dehydrated must not have its ignore mark cleared either, since
    /// that alone costs an interception the file still needs.
    #[tokio::test]
    async fn dehydrate_refuses_an_unmanaged_file_without_calling_the_helper() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = root.path.join("mine.txt");
        std::fs::write(&path, vec![9u8; 1 << 20]).unwrap();

        let error = dehydrate(&link, &root, &path).await.unwrap_err();
        assert!(matches!(error, DehydrateError::NotManaged), "{error:?}");

        assert!(blocks_of(&path) > 64, "an unmanaged file was punched");
        assert!(
            helper.recv_timeout(Duration::from_millis(200)).is_err(),
            "the helper must not be asked anything about a file that may not be dehydrated"
        );
    }

    /// A path is only a request; being inside a registered root
    /// is the authority to empty something. Neither a path outside the root,
    /// nor a symlink inside it pointing out, nor a root whose registration
    /// has gone may reach the punch.
    #[tokio::test]
    async fn dehydrate_refuses_anything_that_is_not_a_file_inside_the_root() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = hydrated_file(elsewhere.path(), "outside.bin", 4096);

        let error = dehydrate(&link, &root, &outside).await.unwrap_err();
        assert!(matches!(error, DehydrateError::OutsideRoot), "{error:?}");
        assert!(blocks_of(&outside) > 0, "a file outside the root was punched");

        let pointer = root.path.join("pointer.bin");
        std::os::unix::fs::symlink(&outside, &pointer).unwrap();
        let error = dehydrate(&link, &root, &pointer).await.unwrap_err();
        assert!(matches!(error, DehydrateError::OutsideRoot), "{error:?}");
        assert!(blocks_of(&outside) > 0, "a symlink walked out of the root");

        let inside = hydrated_file(&root.path, "f.bin", 4096);
        let unregistered = SyncRoot { path: root.path.clone(), root_id: uuid_v4() };
        let error = dehydrate(&link, &unregistered, &inside).await.unwrap_err();
        assert!(matches!(error, DehydrateError::OutsideRoot), "{error:?}");
        assert!(blocks_of(&inside) > 0, "a root that is not registered punched a file");
    }

    // --- Startup recovery -------------------------------------

    /// A file in the state a crash left it in: content on disk, the state
    /// xattr saying what was happening to it, and the stamp a finished
    /// hydration would have written — which §4.4 requires recovery to remove.
    fn interrupted_file(dir: &Path, name: &str, state: State, size: usize) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![3u8; size]).unwrap();
        let file = open_rw(&path);
        write_state(&file, state).unwrap();
        write_stamp(&file).unwrap();
        path
    }

    fn state_of(path: &Path) -> Option<State> {
        read_state(&File::open(path).unwrap()).unwrap()
    }

    /// How many descriptors this process has open right now. Both samples
    /// include the one `read_dir` itself uses, so the difference is the walk's.
    fn open_descriptors() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }

    /// The original proposal's own scenario, extended over a real tree: a crash
    /// mid-hydration and mid-dehydration each leave a file with content that
    /// must not be trusted, at the top of the root and two levels down.
    /// Both are punched back to `online-only` and lose their stamps; a clean
    /// `hydrated` file, an ordinary `online-only` placeholder, a file that is
    /// not ours at all, a symlink and a FIFO are left exactly as found.
    #[tokio::test]
    async fn interrupted_work_is_reset_to_online_only() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let nested = root.path.join("sub");
        std::fs::create_dir(&nested).unwrap();
        let deeper = nested.join("deeper");
        std::fs::create_dir(&deeper).unwrap();

        // One directly in the root: 's own test put all four inside
        // `sub/`, so nothing pinned that the root's own files are walked.
        interrupted_file(&root.path, "top.bin", State::Hydrating, 8192);
        for (name, state) in [
            ("a.bin", State::Hydrating),
            ("b.bin", State::Dehydrating),
            ("c.bin", State::Hydrated),
            ("d.bin", State::OnlineOnly),
        ] {
            interrupted_file(&nested, name, state, 8192);
        }
        interrupted_file(&deeper, "e.bin", State::Dehydrating, 8192);

        // None of these are ours, and none of them may be counted or touched.
        std::fs::write(root.path.join("theirs.txt"), vec![7u8; 8192]).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = interrupted_file(elsewhere.path(), "outside.bin", State::Hydrating, 8192);
        std::os::unix::fs::symlink(&outside, root.path.join("pointer.bin")).unwrap();
        nix::unistd::mkfifo(&root.path.join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 6, reset: 4, failed: 0, skipped: 0, busy: 0, deferred: 0 },
            "six managed files at three levels, four of them interrupted"
        );
        for path in [
            root.path.join("top.bin"),
            nested.join("a.bin"),
            nested.join("b.bin"),
            deeper.join("e.bin"),
        ] {
            let file = File::open(&path).unwrap();
            assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly), "{path:?}");
            assert_eq!(file.metadata().unwrap().len(), 8192, "{path:?}: size preserved");
            assert!(file.metadata().unwrap().blocks() < 64, "{path:?}: content discarded");
            assert_eq!(
                read_stamp(&file).unwrap(),
                None,
                "{path:?}: the stamp described a hydrated file and must not outlive it"
            );
        }
        for (path, state) in [
            (nested.join("c.bin"), Some(State::Hydrated)),
            (nested.join("d.bin"), Some(State::OnlineOnly)),
            (root.path.join("theirs.txt"), None),
        ] {
            assert_eq!(state_of(&path), state, "{path:?}");
            assert!(blocks_of(&path) > 0, "{path:?} was punched and should not have been");
        }
        assert_eq!(state_of(&outside), Some(State::Hydrating), "a symlink led out of the root");
        assert!(blocks_of(&outside) > 0, "a symlink led out of the root");
    }

    #[tokio::test]
    async fn an_empty_root_is_reported_as_nothing_to_do() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        std::fs::create_dir(root.path.join("empty-sub")).unwrap();

        assert_eq!(recover(&link, &root).await.unwrap(), RecoveryReport::default());
        assert!(
            helper.recv_timeout(Duration::from_millis(200)).is_err(),
            "the helper must not be asked anything when there is nothing to recover"
        );
    }

    /// The recovery half of `dehydration_keeps_the_remote_mtime`:
    /// `fallocate` moves the mtime to now. A whole tree of files that
    /// recovery touched then looks locally modified — and every one of them
    /// is a hole full of zeros, which is an upload-over-remote hazard the
    /// moment a delta engine exists. The stamp is gone by then, so
    /// `dehydrate`'s "modified locally" guard is not what saves you.
    #[tokio::test]
    async fn recovery_keeps_the_mtime_of_the_file_it_resets() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "f.bin", State::Dehydrating, 1 << 20);
        let remote = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let file = open_rw(&path);
        file.set_times(std::fs::FileTimes::new().set_modified(remote)).unwrap();
        drop(file);

        assert_eq!(recover(&link, &root).await.unwrap().reset, 1);

        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            remote,
            "recovery must not move the file's mtime to now"
        );
    }

    /// A `hydrating` file whose download left a checkpoint goes back
    /// to `online-only` with the checkpointed prefix and the checkpoint kept;
    /// the next open resumes. One without a checkpoint is reset as before.
    #[tokio::test]
    async fn recovery_keeps_a_checkpointed_prefix_and_empties_the_rest() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());

        let kept = interrupted_file(&root.path, "kept.bin", State::Hydrating, 1 << 20);
        write_progress(&open_rw(&kept), &Progress { ctag: "c1".into(), bytes: 256 * 1024 }).unwrap();
        let reset = interrupted_file(&root.path, "reset.bin", State::Hydrating, 1 << 20);
        let mtime_before = std::fs::metadata(&kept).unwrap().modified().unwrap();

        let report = recover(&link, &root).await.unwrap();
        assert_eq!(report.reset, 2);

        let file = File::open(&kept).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_progress(&file).unwrap(), Some(Progress { ctag: "c1".into(), bytes: 256 * 1024 }));
        assert_eq!(read_stamp(&file).unwrap(), None);
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut File::open(&kept).unwrap(), &mut content).unwrap();
        assert!(content[..256 * 1024].iter().all(|b| *b == 3), "the prefix is kept");
        assert!(content[256 * 1024..].iter().all(|b| *b == 0), "the rest is punched");
        assert_eq!(std::fs::metadata(&kept).unwrap().modified().unwrap(), mtime_before);

        assert_eq!(state_of(&reset), Some(State::OnlineOnly));
        assert!(blocks_of(&reset) < 64);
        assert_eq!(read_progress(&File::open(&reset).unwrap()).unwrap(), None);
    }

    /// Under the read-only lock every file is 0444; recovery must
    /// still reset one, and leave it 0444.
    #[tokio::test]
    async fn recovery_resets_a_locked_file_and_leaves_it_locked() {
        use std::os::unix::fs::PermissionsExt;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "locked.bin", State::Dehydrating, 8192);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let report = recover(&link, &root).await.unwrap();

        assert_eq!((report.reset, report.skipped), (1, 0), "{report:?}");
        assert_eq!(state_of(&path), Some(State::OnlineOnly));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
    }

    /// A file that is not ours is never made writable, not even for a moment,
    /// just because recovery walked past it.
    #[tokio::test]
    async fn recovery_does_not_touch_the_mode_of_a_file_that_is_not_ours() {
        use std::os::unix::fs::MetadataExt;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let theirs = root.path.join("theirs.txt");
        std::fs::write(&theirs, b"x").unwrap();
        std::fs::set_permissions(&theirs, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
        let ctime = |path: &Path| {
            let meta = std::fs::metadata(path).unwrap();
            (meta.ctime(), meta.ctime_nsec())
        };
        let ctime_before = ctime(&theirs);
        recover(&link, &root).await.unwrap();
        assert_eq!(ctime(&theirs), ctime_before, "a chmod changes the ctime");
    }

    /// The recovery half of
    /// `dehydrate_refuses_anything_that_is_not_a_file_inside_the_root`: a
    /// root whose registration has gone is not a root, and nothing inside it
    /// may be emptied on the strength of a path that used to be one.
    #[tokio::test]
    async fn recovery_refuses_a_root_that_is_no_longer_registered() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "f.bin", State::Hydrating, 8192);
        let unregistered = SyncRoot { path: root.path.clone(), root_id: uuid_v4() };

        let error = recover(&link, &unregistered).await.unwrap_err();
        assert!(matches!(error, RecoveryError::NotRegistered(_)), "{error:?}");
        assert!(blocks_of(&path) > 0, "a root that is not registered punched a file");
        assert!(
            helper.recv_timeout(Duration::from_millis(200)).is_err(),
            "the helper must not be asked about a root this daemon does not hold"
        );

        // And the same folder, still registered, is recovered normally.
        assert_eq!(recover(&link, &root).await.unwrap().reset, 1);
    }

    /// at the level of one name: what is opened is decided by the
    /// `fstat` of the descriptor, not by the `d_type` the listing offered.
    /// A symlink is refused by `O_NOFOLLOW` before it resolves anywhere, a
    /// FIFO and a directory are never handed back as files, and an entry on
    /// another filesystem — a bind mount or a removable disk mounted inside
    /// the sync folder — is reported as `Entry::OtherFilesystem` rather than
    /// opened as this root's content, because the helper's own validation is
    /// scoped to the root's device and would not catch a punch across one.
    #[test]
    fn open_entry_refuses_anything_that_is_not_a_regular_file_on_the_root_device() {
        let dir = tempfile::tempdir().unwrap();
        let handle = File::open(dir.path()).unwrap();
        let dev = handle.metadata().unwrap().dev();

        std::fs::write(dir.path().join("f.bin"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("f.bin"), dir.path().join("link")).unwrap();
        nix::unistd::mkfifo(&dir.path().join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        let of = |name: &str| {
            let kind = std::fs::symlink_metadata(dir.path().join(name)).unwrap().file_type();
            (OsString::from(name), kind)
        };

        let (name, kind) = of("f.bin");
        assert!(matches!(open_entry(&handle, &name, kind, dev).unwrap(), Entry::File(..)));
        assert!(
            matches!(open_entry(&handle, &name, kind, dev + 1).unwrap(), Entry::OtherFilesystem),
            "a file on another filesystem must be reported, not silently opened as this root's \
             content"
        );

        let (name, kind) = of("sub");
        assert!(matches!(open_entry(&handle, &name, kind, dev).unwrap(), Entry::Directory(_)));
        assert!(matches!(
            open_entry(&handle, &name, kind, dev + 1).unwrap(),
            Entry::OtherFilesystem
        ));

        for name in ["link", "pipe"] {
            let (name, kind) = of(name);
            assert!(
                matches!(open_entry(&handle, &name, kind, dev).unwrap(), Entry::Elsewhere),
                "{name:?}"
            );
            // And the same entry lied about, as a mid-walk swap would: the
            // listing said "regular file", the thing on disk is not one.
            let lying = std::fs::metadata(dir.path().join("f.bin")).unwrap().file_type();
            assert!(
                matches!(open_entry(&handle, &name, lying, dev).unwrap(), Entry::Elsewhere),
                "{name:?} was accepted as a file because the listing claimed it was one"
            );
        }
    }

    /// The recovery twin of
    /// `dehydrate_punches_the_file_it_checked_even_if_the_name_is_taken_over`
    /// — the exact defect that destroyed 300 KiB of real data three runs out
    /// of three, in the one place that would reintroduce it invisibly. The
    /// file is replaced while recovery waits for the helper's `ClearIgnore`
    /// ack; a by-path reopen after that point punches the replacement, which
    /// was never classified, carries no konedrive xattr, and is somebody's
    /// fresh work.
    #[tokio::test]
    async fn recovery_punches_the_file_it_classified_even_if_the_name_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "f.bin", State::Hydrating, 300 * 1024);
        // Both of these live outside the root, so that the walk has exactly
        // one name to look at and the outcome cannot depend on the order the
        // directory happens to be read in. `original` is a second name for
        // the same inode, so the file the walk classified stays reachable
        // after the swap; a hard link is not an open descriptor, so it does
        // not disturb the write lease.
        let staging = tempfile::tempdir().unwrap();
        let original = staging.path().join("original.link");
        std::fs::hard_link(&path, &original).unwrap();
        let swap_in = staging.path().join("replacement.tmp");
        std::fs::write(&swap_in, vec![0xABu8; 300 * 1024]).unwrap();

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let swapped_to = path.clone();
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            std::fs::rename(&swap_in, &swapped_to).unwrap();
        });
        let link = connected(&socket_path).await;

        let report = recover(&link, &root).await.unwrap();
        assert_eq!(report, RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 0, busy: 0, deferred: 0 });

        let replacement = std::fs::read(&path).unwrap();
        assert!(
            replacement.iter().all(|b| *b == 0xAB),
            "the replacement file was punched: {} of its {} bytes are zero",
            replacement.iter().filter(|b| **b == 0).count(),
            replacement.len()
        );
        assert!(blocks_of(&path) > 64, "the replacement file lost its blocks");
        assert_eq!(
            state_of(&path),
            None,
            "a file konedrive never managed was stamped by the recovery"
        );

        assert!(blocks_of(&original) < 64, "the file that was classified was not punched");
        assert_eq!(state_of(&original), Some(State::OnlineOnly));
    }

    /// Reproduced: `sub/` is replaced by a symlink to a
    /// directory outside the root while recovery waits for a `ClearIgnore`
    /// ack. A walk that re-resolves subdirectory paths follows it and empties
    /// a file that was never inside any sync root —
    /// `report=RecoveryReport { reset: 2, scanned: 2 }`, `victim blocks=0
    /// state=Some(OnlineOnly)`. The helper does not back this out: its check
    /// is same-uid-same-filesystem, which any file in the user's home passes.
    ///
    /// Either order of the two names in the root defeats the attack now: if
    /// `sub` is reached first the walk already holds its descriptor, and if
    /// `top.bin` is reached first the swapped-in symlink is refused by
    /// `O_NOFOLLOW`.
    #[tokio::test]
    async fn recovery_stays_inside_the_root_when_a_directory_is_swapped_mid_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let nested = root.path.join("sub");
        std::fs::create_dir(&nested).unwrap();
        interrupted_file(&root.path, "top.bin", State::Hydrating, 8192);
        interrupted_file(&nested, "inside.bin", State::Hydrating, 8192);

        let elsewhere = tempfile::tempdir().unwrap();
        let victim = interrupted_file(elsewhere.path(), "victim.bin", State::Hydrating, 8192);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let decoy = elsewhere.path().to_path_buf();
        let swapped = nested.clone();
        let stashed = root.path.join("sub.stashed");
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            std::fs::rename(&swapped, &stashed).unwrap();
            std::os::unix::fs::symlink(&decoy, &swapped).unwrap();
        });
        let link = connected(&socket_path).await;

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            state_of(&victim),
            Some(State::Hydrating),
            "a file outside the root was relabelled by recovery: {report:?}"
        );
        assert!(
            blocks_of(&victim) > 0,
            "a file outside the root was emptied by recovery: {report:?}"
        );
    }

    /// `list_dir(dir).await?` and `entry?` used to propagate out
    /// of `recover`, so one mode-`000` subdirectory returned `Err(EACCES)`
    /// for the whole root: the count of what had already been punched was
    /// lost and no sibling subtree was ever visited. A directory removed
    /// while the daemon starts did the same with `ENOENT`, which is a
    /// routine race, not a corruption. Recovery is the one component whose
    /// entire job is coping with a messy on-disk state.
    #[tokio::test]
    async fn an_unreadable_subdirectory_does_not_stop_the_walk() {
        use std::os::unix::fs::PermissionsExt;

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let locked = root.path.join("locked");
        std::fs::create_dir(&locked).unwrap();
        interrupted_file(&locked, "hidden.bin", State::Hydrating, 8192);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let sibling = root.path.join("sibling");
        std::fs::create_dir(&sibling).unwrap();
        let reachable = interrupted_file(&sibling, "reachable.bin", State::Hydrating, 8192);

        let report = recover(&link, &root).await.unwrap();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 },
            "the sibling subtree must still be recovered, and the unreachable directory said so"
        );
        assert_eq!(state_of(&reachable), Some(State::OnlineOnly));
        assert!(blocks_of(&reachable) < 64);
        assert_eq!(
            state_of(&locked.join("hidden.bin")),
            Some(State::Hydrating),
            "what could not be reached must be left for the next start"
        );
    }

    /// The other way a directory goes unread: it was opened while it was
    /// readable and stopped being readable before it was listed. The listing
    /// goes through `/proc/self/fd/<n>`, which re-checks permission on the
    /// inode, so this is a real outcome rather than a theoretical one — and
    /// like every other unreachable thing it is counted, and the walk
    /// continues with whatever else is on the stack.
    #[tokio::test]
    async fn a_directory_that_cannot_be_listed_is_counted_and_does_not_end_the_walk() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        interrupted_file(&sub, "inside.bin", State::Hydrating, 8192);
        let handle = File::open(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut stack = Vec::new();
        let mut report = RecoveryReport::default();
        descend(&mut stack, Arc::new(handle), sub.clone(), 0, &mut report).await;

        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(stack.is_empty(), "a directory that could not be listed must not be walked");
        assert_eq!(report, RecoveryReport { scanned: 0, reset: 0, failed: 0, skipped: 1, busy: 0, deferred: 0 });
    }

    /// other half. A file that cannot be opened was `Err(_) =>
    /// continue`: no error, no log, no count — which is how 991 files stayed
    /// `hydrating` with untrusted content while the report said
    /// `reset: 1009, scanned: 1009`. Whatever the reason (permissions,
    /// `EMFILE`, a race with deletion), a file recovery could not look at may
    /// be hiding an interrupted one, and the report has to say so.
    #[tokio::test]
    async fn a_file_that_cannot_be_opened_is_counted_not_passed_over_in_silence() {
        use std::os::unix::fs::PermissionsExt;

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let locked = interrupted_file(&root.path, "locked.bin", State::Hydrating, 8192);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let reachable = interrupted_file(&root.path, "reachable.bin", State::Hydrating, 8192);

        let report = recover(&link, &root).await.unwrap();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(report, RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 });
        assert_eq!(state_of(&reachable), Some(State::OnlineOnly));
        assert_eq!(state_of(&locked), Some(State::Hydrating));
    }

    /// Open, decide, punch, close — one file at a time. The
    /// version this replaces opened every regular file in a directory
    /// `O_RDWR`, whatever its state, and held all of those descriptors until
    /// the directory was finished; with `RLIMIT_NOFILE` at systemd's default
    /// of 1024 that silently defeated recovery of a large folder, healthy or
    /// not. Measured from inside the walk — the helper's hook runs while
    /// recovery is blocked on the `ClearIgnore` ack for the one interrupted
    /// file, which is the moment the old version was holding all 400 of the
    /// others.
    #[tokio::test]
    async fn recovery_holds_one_file_open_at_a_time() {
        const BYSTANDERS: usize = 400;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        for i in 0..BYSTANDERS {
            interrupted_file(&root.path, &format!("hydrated-{i}.bin"), State::Hydrated, 64);
        }
        interrupted_file(&root.path, "interrupted.bin", State::Dehydrating, 64);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let during = Arc::new(AtomicUsize::new(0));
        let sampler = Arc::clone(&during);
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            sampler.store(open_descriptors(), Ordering::SeqCst);
        });
        let link = connected(&socket_path).await;

        let before = open_descriptors();
        let report = recover(&link, &root).await.unwrap();
        assert_eq!(report.reset, 1, "{report:?}");
        assert_eq!(report.scanned, BYSTANDERS + 1, "{report:?}");

        let held = during.load(Ordering::SeqCst).saturating_sub(before);
        assert!(
            held < 64,
            "{held} more descriptors were open mid-walk than before it, with {BYSTANDERS} \
             bystander files in the directory: the walk is holding a descriptor per file, so a \
             large folder exhausts the table and the rest of it is skipped in silence"
        );
    }

    /// Streaming has a second consequence worth pinning: because only one
    /// file is open at a time, everything else in the directory is still
    /// just a name when the walk is waiting on the helper. Here both files
    /// are deleted at that moment — the one being recovered continues on its
    /// descriptor, and the one that was never reached is counted rather than
    /// ending the walk. A user deleting a folder while the daemon starts is
    /// routine.
    #[tokio::test]
    async fn a_file_that_disappears_mid_walk_is_counted_and_the_walk_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let first = interrupted_file(&root.path, "one.bin", State::Hydrating, 8192);
        let second = interrupted_file(&root.path, "two.bin", State::Hydrating, 8192);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            std::fs::remove_file(&first).unwrap();
            std::fs::remove_file(&second).unwrap();
        });
        let link = connected(&socket_path).await;

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 },
            "one file was open and was finished; the other was still only a name"
        );
    }

    /// `read_state(&file).unwrap_or(None)` collapsed `Corrupt` and genuine
    /// I/O errors into "not one of ours". Safe in direction — a file whose
    /// state cannot be read must never be punched — but the file was then
    /// not counted, not logged and never noticed, while §5.2 has the helper
    /// deny every open of it with `EIO` for as long as it stays that way.
    #[tokio::test]
    async fn recovery_never_punches_a_file_whose_state_it_cannot_read() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "corrupt.bin", State::Hydrating, 8192);
        open_rw(&path).set_xattr("user.konedrive.state", b"hydratin").unwrap();

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 0, reset: 0, failed: 0, skipped: 1, busy: 0, deferred: 0 },
            "a managed file in an unknown state is neither ours to punch nor ours to ignore"
        );
        assert!(blocks_of(&path) > 0, "a file whose state could not be read was punched");
        assert!(
            helper.recv_timeout(Duration::from_millis(200)).is_err(),
            "nothing may be asked of the helper about a file that must not be touched"
        );
    }

    /// `punch_clean_file` takes a write lease before it empties
    /// anything, so that an application opening the file mid-punch is
    /// suspended by the kernel instead of reading blocks as they go away.
    /// The window is wider at startup, not narrower: the helper's
    /// `register_root` walk must mark the whole tree before anything is
    /// intercepted at all, so until it finishes any thumbnailer, backup or
    /// indexer can hold an interrupted file open while this runs. A refusal
    /// means exactly that, and the file waits for the next start — or for
    /// the open that has it, which fills it. It is counted busy, not failed.
    #[tokio::test]
    async fn recovery_leaves_a_file_that_is_in_use_for_the_next_start() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "busy.bin", State::Hydrating, 1 << 20);
        let held_open = File::open(&path).unwrap();

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 0, failed: 0, skipped: 0, busy: 1, deferred: 0 },
            "a file in use is busy, not a failure to recover (the final review's m11): the next \
             open fills it, and the next start resets it if it is still interrupted"
        );
        assert_eq!(
            state_of(&path),
            Some(State::Hydrating),
            "a file nobody could take a lease on must be left exactly as found"
        );
        assert!(blocks_of(&path) > 64, "a file open in another process was emptied under it");

        // And once it is closed, the next start finishes the job.
        drop(held_open);
        assert_eq!(recover(&link, &root).await.unwrap().reset, 1);
        assert_eq!(state_of(&path), Some(State::OnlineOnly));
    }

    /// An open file of the inode that is on its way out — here one closed
    /// 30 ms after the helper is asked to clear the mark, so it is still
    /// there when recovery first asks for the lease — does not make the file
    /// `busy`: the refusal is retried briefly (`lease_retrying_briefly`). A
    /// process being spawned holds exactly such a copy of the walk's own
    /// read-only descriptor until its `exec`.
    #[tokio::test]
    async fn recovery_waits_out_an_open_that_is_about_to_close() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "x.bin", State::Hydrating, 8192);
        let opened = path.clone();
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            let held = File::open(&opened).unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                drop(held);
            });
        });
        let link = connected(&socket_path).await;

        let report = recover(&link, &root).await.unwrap();

        assert_eq!((report.reset, report.busy), (1, 0), "{report:?}");
        assert_eq!(state_of(&path), Some(State::OnlineOnly));
    }

    /// After a reconnect the previous
    /// connection's fills keep running while the new connection's recovery
    /// walks, and recovery read a file's state only when it opened it. A fill
    /// that commits `hydrated` — and closes its descriptor — between
    /// recovery's `ClearIgnore` and its lease left recovery punching a
    /// complete, `hydrated` file; in the VM an opener had the file
    /// ignore-marked in that gap, and the next reader got 65 536 zero bytes
    /// after no fetch. The fake helper commits the fill when it is asked to
    /// clear the mark, which is exactly that gap.
    #[tokio::test]
    async fn recovery_does_not_punch_a_file_a_fill_finished_after_it_looked() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "x.bin", State::Hydrating, 1 << 20);
        let committed = path.clone();
        let _helper = fake_helper(socket_path.clone(), 0, move || {
            let file = open_rw(&committed);
            write_stamp(&file).unwrap();
            write_state(&file, State::Hydrated).unwrap();
            file.sync_all().unwrap();
        });
        let link = connected(&socket_path).await;

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            state_of(&path),
            Some(State::Hydrated),
            "recovery changed the state of a file whose fill committed while it waited ({report:?})"
        );
        assert!(
            blocks_of(&path) > 64,
            "recovery punched a file whose fill had committed `hydrated` after recovery read it \
             `hydrating` — the helper lets every opener of a `hydrated` file through, and may have \
             ignore-marked it ({report:?})"
        );
        assert_eq!(report.reset, 0, "{report:?}");
    }

    /// The lock: every fill and every free-up of this daemon
    /// holds the per-inode lock for as long as it works on the file, and a
    /// fill from the previous connection is still one of them. Recovery must
    /// not touch a file whose lock is held — it is being filled or freed up
    /// right now — and must not wait for it either, or a reconnect would wait
    /// for a download of any length. The fill here has already
    /// closed its descriptor, so only the lock stands between it and the
    /// punch.
    #[tokio::test]
    async fn recovery_leaves_a_file_this_daemon_is_filling_to_the_fill() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "x.bin", State::Hydrating, 1 << 20);
        let locks = InodeLocks::new();
        let fill = locks.lock(InodeKey::of(&open_rw(&path)).unwrap()).await;

        let report = tokio::time::timeout(
            Duration::from_secs(5),
            super::recover(&Clearance::Link(link.clone()), &root, &locks),
        )
        .await
        .expect("recovery waited for a fill of the same file")
        .unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 0, failed: 0, skipped: 0, busy: 1, deferred: 0 },
            "a file this daemon is filling is busy, and left to the fill"
        );
        assert_eq!(state_of(&path), Some(State::Hydrating));
        assert!(blocks_of(&path) > 64, "recovery punched a file a fill of this daemon held");

        drop(fill);
        let report = super::recover(&Clearance::Link(link), &root, &locks).await.unwrap();
        assert_eq!(report.reset, 1, "with the fill gone, the interrupted file is reset");
    }

    /// Point 2/3 of the task brief, and invariant M3: a file left
    /// `dehydrating` by a crash between `write_state(Dehydrating)` and a
    /// successful `ClearIgnore` may still carry its ignore mark. Recovery
    /// must ask the helper to clear it — on the very descriptor it is about
    /// to punch — exactly as `dehydrate` does, never skip straight to the
    /// punch on the strength of the state xattr alone.
    #[tokio::test]
    async fn recovery_clears_the_ignore_mark_before_punching() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "a.bin", State::Hydrating, 8192);
        let expected_ino = std::fs::metadata(&path).unwrap().ino();

        recover(&link, &root).await.unwrap();

        let seen = asked_to(&helper, "ClearIgnore");
        assert_eq!(seen.ino, expected_ino, "the helper was handed a different file");
        assert_eq!(
            seen.state,
            Some(State::Hydrating),
            "the mark was cleared on a descriptor that is not the interrupted file"
        );
    }

    /// The worst outcome in this project, guarded against here exactly as
    /// `dehydrate_leaves_the_file_untouched_when_clear_ignore_fails` guards
    /// it there: a file whose `ClearIgnore` is refused must come out of
    /// recovery completely untouched, not punched. Punching a file whose
    /// ignore mark could not be confirmed cleared would leave it empty and
    /// permanently un-intercepted, reading as zeros forever, with recovery
    /// itself reporting success.
    #[tokio::test]
    async fn recovery_leaves_a_file_untouched_when_clear_ignore_fails() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), libc::EIO, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "a.bin", State::Dehydrating, 8192);

        let report = recover(&link, &root).await.unwrap();
        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 0, failed: 1, skipped: 0, busy: 0, deferred: 0 },
            "a file whose ignore mark could not be cleared must be counted as failed, not as \
             reset and not as nothing at all"
        );

        let after = File::open(&path).unwrap();
        assert_eq!(
            read_state(&after).unwrap(),
            Some(State::Dehydrating),
            "left exactly as found, so the next start retries it"
        );
        assert!(
            after.metadata().unwrap().blocks() > 0,
            "must not be punched when ClearIgnore failed"
        );
    }

    /// A refusal has to keep its kind all the way out of
    /// `reset_interrupted`. `Result<(), String>` flattened `Refused`,
    /// `Timeout`, `NotRunning` and `ENOSPC` into one text field, and those
    /// are four different situations with four different answers — retry
    /// now, retry later, start the helper, free some space.
    #[tokio::test]
    async fn a_refusal_keeps_its_kind() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), libc::EPERM, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "a.bin", State::Dehydrating, 8192);

        let error = reset_interrupted(&Clearance::Link(link.clone()), open_rw(&path)).await.unwrap_err();
        assert!(
            matches!(error, ResetError::Helper(HelperError::Refused(libc::EPERM))),
            "{error:?}"
        );
    }

    /// The recovery half of
    /// `the_punch_is_made_durable_before_the_file_is_called_online_only`. A
    /// punch that is only in page cache, published as `online-only`, is a
    /// file the next boot can find with its blocks back and its state
    /// insisting they are gone — and nothing will ever hydrate it, because
    /// `online-only` is exactly the state that means "the content is
    /// elsewhere". Recovery is the code that runs *after* that next boot, so
    /// it is the last thing that should leave one behind.
    #[test]
    fn the_recovery_punch_is_made_durable_before_the_file_is_called_online_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "f.bin", State::Dehydrating, 1 << 20);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});

        let asked = root.clone();
        let denied = &[libc::SYS_fsync, libc::SYS_fdatasync];
        let report = with_syscalls_denied(denied, move || async move {
            let link = connected(&socket_path).await;
            recover(&link, &asked).await.unwrap()
        });

        let Some(report) = report else {
            eprintln!("seccomp is unavailable here; skipping the durability check");
            return;
        };
        assert_eq!(report, RecoveryReport { scanned: 1, reset: 0, failed: 1, skipped: 0, busy: 0, deferred: 0 });
        assert!(blocks_of(&path) < 64, "the punch itself should still have happened");
        assert_eq!(
            state_of(&path),
            Some(State::Dehydrating),
            "a file whose punch could not be made durable must not be published as online-only"
        );
    }

    /// The other failure calls out: a `dehydrating` file whose
    /// `ClearIgnore` succeeds and whose punch then fails. Nothing may be
    /// published, nothing may be counted as reset, and the file waits for
    /// the next start — `fallocate` failing is `ENOSPC` on a filesystem with
    /// no room for the metadata a hole needs, or `EOPNOTSUPP` on one that
    /// cannot punch at all.
    #[test]
    fn a_punch_that_fails_leaves_the_file_for_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let path = interrupted_file(&root.path, "f.bin", State::Dehydrating, 1 << 20);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = fake_helper(socket_path.clone(), 0, || {});

        let asked = root.clone();
        let report = with_syscalls_denied(&[libc::SYS_fallocate], move || async move {
            let link = connected(&socket_path).await;
            recover(&link, &asked).await.unwrap()
        });

        let Some(report) = report else {
            eprintln!("seccomp is unavailable here; skipping the failed-punch check");
            return;
        };
        assert_eq!(report, RecoveryReport { scanned: 1, reset: 0, failed: 1, skipped: 0, busy: 0, deferred: 0 });
        assert!(blocks_of(&path) > 64, "the punch failed, so the blocks must still be there");
        assert_eq!(
            state_of(&path),
            Some(State::Dehydrating),
            "a file that was not emptied must not be called online-only"
        );
        assert!(
            read_stamp(&File::open(&path).unwrap()).unwrap().is_some(),
            "nothing may be published about a file the punch did not reach"
        );
        asked_to(&helper, "ClearIgnore");
    }

    // --- MAX_DEPTH, the socket disproof, and the xdev split --------------

    /// `count` nested directories under `root`, returning the deepest one.
    /// `root` itself is nesting level 0, so the returned directory is at
    /// level `count` — the same convention [`MAX_DEPTH`] and `Frame::depth`
    /// use.
    fn nested_dirs(root: &Path, count: usize) -> PathBuf {
        let mut path = root.to_path_buf();
        for i in 0..count {
            path = path.join(format!("d{i}"));
            std::fs::create_dir(&path).unwrap();
        }
        path
    }

    /// The helper refuses to *mark* anything past `MAX_DEPTH` levels
    /// (`crates/konedrive-helper/src/marks.rs`), so a directory below it is
    /// unmarked and uninterceptable no matter what recovery finds there —
    /// the two halves have to agree on the same number, which is why both
    /// import the one `konedrive_fs::MAX_DEPTH`. A file at level 127
    /// (inside the deepest directory the helper would still have marked) is
    /// recovered normally; a directory at level 128 is never even listed,
    /// counted in `skipped` instead, and whatever is inside it — a second
    /// interrupted file — is never seen at all, not even as a separate
    /// `skipped` entry, because the whole directory was refused as one.
    #[tokio::test]
    async fn recovery_stops_at_the_same_depth_the_helper_stops_marking() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());

        let level_127 = nested_dirs(&root.path, MAX_DEPTH - 1);
        interrupted_file(&level_127, "shallow.bin", State::Dehydrating, 4096);

        let level_128 = level_127.join("too-deep");
        std::fs::create_dir(&level_128).unwrap();
        interrupted_file(&level_128, "deep.bin", State::Dehydrating, 4096);

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 1, busy: 0, deferred: 0 },
            "level 127 must be recovered normally; level 128 must be refused as one directory, \
             not silently skipped and not walked into"
        );
        assert!(
            blocks_of(&level_128.join("deep.bin")) > 0,
            "a file deeper than the helper would ever mark must not be punched"
        );
    }

    /// The previous round called deleting this function's `d_type` guard —
    /// the `else { return Ok(Entry::Elsewhere) }` arm below, taken whenever
    /// `kind` is neither a directory nor a regular file — an *equivalent*
    /// mutant: `O_NOFOLLOW` stops a symlink and the post-open `fstat` stops
    /// a FIFO, independently of the guard, which is true as far as it goes.
    /// It is not equivalent, though. `open(2)` on a **Unix domain socket**
    /// returns `ENXIO` — not `ELOOP`, not a successful open of something the
    /// `fstat` then rejects — so with the guard gone that errno reaches
    /// [`recover`]'s `Err(e) => report.skipped += 1` arm instead of the
    /// silent `Entry::Elsewhere` every other non-file, non-directory entry
    /// gets. `RecoveryReport::skipped` is documented as "a non-zero value
    /// means the root was **not** fully recovered", so that is an observable
    /// change in a `pub` field, not an equivalent mutant.
    ///
    /// A dangling symlink and a FIFO sit beside the socket because they are
    /// the two kinds the earlier round's reasoning actually covers — the
    /// point of running them together is that only the socket's count moves.
    /// Measured against this test by hand: unmutated,
    /// `RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 0, busy: 0, deferred: 0 }`; with
    /// the guard's `else` arm deleted (folding every non-directory kind into
    /// the same open the regular-file branch uses), `skipped: 1`.
    #[tokio::test]
    async fn a_unix_socket_is_silently_elsewhere_not_counted_as_skipped() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = fake_helper(socket_path.clone(), 0, || {});
        let link = connected(&socket_path).await;

        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        interrupted_file(&root.path, "f.bin", State::Dehydrating, 4096);

        // The control: both already established as unaffected either way.
        std::os::unix::fs::symlink(root.path.join("nowhere"), root.path.join("dangling"))
            .unwrap();
        nix::unistd::mkfifo(&root.path.join("pipe"), Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        // A real AF_UNIX socket special file: bound, never connected, whose
        // descriptor is dropped immediately — the directory entry it leaves
        // behind persists exactly like a closed regular file's would.
        let sock_fd =
            socket(AddressFamily::Unix, SockType::Stream, SockFlag::SOCK_CLOEXEC, None).unwrap();
        bind(sock_fd.as_raw_fd(), &UnixAddr::new(&root.path.join("sock")).unwrap()).unwrap();
        drop(sock_fd);

        let report = recover(&link, &root).await.unwrap();

        assert_eq!(
            report,
            RecoveryReport { scanned: 1, reset: 1, failed: 0, skipped: 0, busy: 0, deferred: 0 },
            "a socket must be silently Elsewhere, exactly like the symlink and the FIFO beside \
             it — not counted in skipped"
        );
    }

    /// `st_dev` check does its job — [`open_entry`] never opens
    /// across it — but until [`Entry::OtherFilesystem`] existed, the whole
    /// excluded subtree vanished into the same silent `Entry::Elsewhere` a
    /// symlink gets: `skipped == 0` while a real subtree went unrecovered,
    /// contradicting `RecoveryReport::skipped`'s own doc comment ("a
    /// non-zero value means the root was **not** fully recovered").
    ///
    /// Reproducing a genuine cross-device boundary unprivileged needs a real
    /// mount, which needs a mount namespace this test process does not have.
    /// Acquiring one from inside an already multi-threaded `cargo test`
    /// binary is refused by the kernel outright (`unshare(CLONE_NEWUSER)`
    /// requires a single-threaded caller), so this test re-executes its own
    /// binary as a fresh, single-threaded process under `unshare -Urm`
    /// instead. `KONEDRIVE_XDEV_CHILD` is what tells that re-exec apart from
    /// the original run: only the child mounts a tmpfs, places one
    /// interrupted file under it, runs [`recover`], and turns its own
    /// assertions into the process's exit status for the parent half to
    /// check.
    #[test]
    fn a_subtree_on_another_filesystem_is_counted_and_logged() {
        if std::env::var_os("KONEDRIVE_XDEV_CHILD").is_some() {
            xdev_child();
            return;
        }

        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                eprintln!("cannot find this test binary ({e}); skipping the xdev check");
                return;
            }
        };
        let invocation = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "--"])
            .arg(&exe)
            .args([
                "sync::root::tests::a_subtree_on_another_filesystem_is_counted_and_logged",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("KONEDRIVE_XDEV_CHILD", "1")
            .output();
        let output = match invocation {
            Ok(output) => output,
            Err(e) => {
                eprintln!("`unshare` is unavailable here ({e}); skipping the xdev check");
                return;
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success()
            && (stderr.contains("unshare failed")
                || stderr.contains("Operation not permitted")
                || stderr.contains("Permission denied"))
        {
            eprintln!(
                "unprivileged user namespaces are unavailable here; skipping the xdev check: \
                 {stderr}"
            );
            return;
        }
        assert!(
            output.status.success(),
            "the cross-device recovery check failed:\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
    }

    /// The half of
    /// [`a_subtree_on_another_filesystem_is_counted_and_logged`] that
    /// actually runs under `unshare -Urm`: a real tmpfs mounted a level
    /// inside the sync root, and one interrupted file under that mount.
    /// Panicking here fails the re-exec'd process, which the parent half
    /// reports.
    fn xdev_child() {
        let dir = tempfile::tempdir().unwrap();
        let root = test_root(dir.path());
        let mount_point = root.path.join("mnt");
        std::fs::create_dir(&mount_point).unwrap();
        nix::mount::mount(
            Some("tmpfs"),
            &mount_point,
            Some("tmpfs"),
            nix::mount::MsFlags::empty(),
            None::<&str>,
        )
        .expect("mounting a tmpfs inside the sync root under unshare -Urm");

        let victim = interrupted_file(&mount_point, "victim.bin", State::Dehydrating, 8192);

        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let _helper = fake_helper(socket_path.clone(), 0, || {});
            let link = connected(&socket_path).await;

            let report = recover(&link, &root).await.unwrap();
            assert_eq!(
                report,
                RecoveryReport { scanned: 0, reset: 0, failed: 0, skipped: 1, busy: 0, deferred: 0 },
                "a subtree on another filesystem must be counted in skipped, not silently \
                 passed over: {report:?}"
            );
            assert!(blocks_of(&victim) > 0, "a file on another filesystem must not be punched");
            assert_eq!(
                state_of(&victim),
                Some(State::Dehydrating),
                "a file on another filesystem must be left exactly as found"
            );
        });
    }
}
