//! A placeholder is an ordinary sparse file whose state lives in its xattrs.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::str::FromStr;
use std::time::SystemTime;

use nix::fcntl::{AtFlags, FallocateFlags, OFlag};
use nix::sys::stat::Mode;
use xattr::FileExt;

pub const XATTR_ITEM_ID: &str = "user.konedrive.item-id";
pub const XATTR_STATE: &str = "user.konedrive.state";
pub const XATTR_STAMP: &str = "user.konedrive.stamp";
pub const XATTR_ROOT: &str = "user.konedrive.root";
/// On a OneDrive folder's root directory: the drive id of the account it shows. A folder
/// forgotten by one account is refused to another that is not the same drive.
pub const XATTR_DRIVE: &str = "user.konedrive.drive";
pub const XATTR_CTAG: &str = "user.konedrive.ctag";
pub const XATTR_PROGRESS: &str = "user.konedrive.progress";
/// "Always keep on this device": `"1"` on a pinned file or folder. A folder's
/// pin covers everything under it. The attribute is the only record of a
/// pin, so a pin survives a rebuild of the daemon's tree store, and the
/// Dolphin plugin reads it directly.
pub const XATTR_PIN: &str = "user.konedrive.pin";
/// On a file with a change waiting to go up (`docs/design/writes.md` §11): `pending`,
/// `uploading` or `blocked`. The daemon writes it and takes it off at the
/// commit; Dolphin's emblem plugin reads it as it reads the state.
pub const XATTR_SYNC: &str = "user.konedrive.sync";

/// A folder under the read phase's read-only lock, and one without.
pub const LOCKED_FILE_MODE: u32 = 0o444;
pub const LOCKED_DIR_MODE: u32 = 0o555;
pub const OPEN_FILE_MODE: u32 = 0o644;
pub const OPEN_DIR_MODE: u32 = 0o755;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    OnlineOnly,
    Hydrating,
    Hydrated,
    Dehydrating,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnlineOnly => "online-only",
            Self::Hydrating => "hydrating",
            Self::Hydrated => "hydrated",
            Self::Dehydrating => "dehydrating",
        }
    }
}

impl FromStr for State {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, ()> {
        match value {
            "online-only" => Ok(Self::OnlineOnly),
            "hydrating" => Ok(Self::Hydrating),
            "hydrated" => Ok(Self::Hydrated),
            "dehydrating" => Ok(Self::Dehydrating),
            _ => Err(()),
        }
    }
}

/// Combines results from an operation and a mode restoration, ensuring both
/// errors are reported: if op() fails and restore also
/// fails, both failures are in the error message.
pub fn combine_op_and_restore_results<T>(
    op_result: io::Result<T>,
    restore_result: io::Result<()>,
    mode: u32,
) -> io::Result<T> {
    match (op_result, restore_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(e)) => Err(io::Error::new(
            e.kind(),
            format!("the write succeeded, but the mode could not be put back to {mode:o}: {e}"),
        )),
        (Err(op_err), Ok(())) => Err(op_err),
        (Err(op_err), Err(restore_err)) => Err(io::Error::new(
            op_err.kind(),
            format!("{op_err}; and the mode could not be put back to {mode:o}: {restore_err}"),
        )),
    }
}

/// Runs `op` with the owner's write permission on `file`, putting the mode
/// back afterwards when it had to be lifted.
///
/// A `user.*` attribute can only be written by someone with write permission
/// on the inode — the owner included, however the descriptor was opened
/// (measured: the owner's `setfattr` on a `0444` file fails `EACCES`). Under
/// the lock every file is `0444`, so every attribute the daemon writes goes
/// through here, and the window is that one call. A mode that cannot be put
/// back is an error even when `op` succeeded: the lock would silently be off.
pub fn with_owner_write<T>(file: &File, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode() & 0o7777;
    if mode & 0o200 != 0 {
        return op();
    }
    set_mode(file, mode | 0o200)?;
    let result = op();
    let restored = set_mode(file, mode);
    combine_op_and_restore_results(result, restored, mode)
}

pub fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    nix::sys::stat::fchmod(file.as_fd(), Mode::from_bits_truncate(mode))?;
    Ok(())
}

fn set_xattr(file: &File, name: &str, value: &[u8]) -> io::Result<()> {
    with_owner_write(file, || file.set_xattr(name, value))
}

fn remove_xattr(file: &File, name: &str) -> io::Result<()> {
    with_owner_write(file, || match file.remove_xattr(name) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
        Err(e) => Err(e),
    })
}

/// A descriptor somebody else owns, for the `xattr` crate, whose calls are on a trait that
/// only `File` has: a reader then needs no `File` of its own, and so no duplicate.
struct Lent<'a>(BorrowedFd<'a>);

impl AsRawFd for Lent<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl FileExt for Lent<'_> {}

/// Reads through the descriptor it is given and opens nothing: the helper reads an
/// intercepted open's attributes through the event's own descriptor.
fn read_xattr(file: &impl AsFd, name: &str) -> io::Result<Option<String>> {
    // `xattr::FileExt::get_xattr` already turns a missing attribute
    // (ENODATA/ENOATTR) into `Ok(None)` internally (see the `xattr` crate's
    // `extract_noattr`), so there is no ENODATA `Err` case left to match
    // here.
    match Lent(file.as_fd()).get_xattr(name) {
        Ok(Some(raw)) => Ok(Some(String::from_utf8_lossy(&raw).into_owned())),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Why `read_state` could not tell the caller what state a file is in.
///
/// `Corrupt` exists because collapsing it into `Ok(None)` — "this file is not
/// one of ours" — is a data-loss bug, not a simplification. The helper's
/// decision table maps `Ok(None)` to `FAN_ALLOW`, so a placeholder
/// whose `user.konedrive.state` had been truncated, half-written or set by
/// hand to something we do not recognise would be handed to the application
/// with its body still missing: zeros where the content should be. The whole
/// point of §5.2's last rule is that an unreadable or unintelligible state on
/// a file that looks managed must deny, never allow.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("cannot read {XATTR_STATE}: {0}")]
    Io(#[from] io::Error),
    #[error("{XATTR_STATE} holds an unrecognised value {0:?}")]
    Corrupt(String),
}

/// `Ok(None)` means, and only means, that the attribute is absent — the file
/// carries no `user.konedrive.state` at all.
pub fn read_state(file: &impl AsFd) -> Result<Option<State>, StateError> {
    match read_xattr(file, XATTR_STATE)? {
        None => Ok(None),
        Some(value) => match value.parse() {
            Ok(state) => Ok(Some(state)),
            Err(()) => Err(StateError::Corrupt(value)),
        },
    }
}

pub fn write_state(file: &File, state: State) -> io::Result<()> {
    set_xattr(file, XATTR_STATE, state.as_str().as_bytes())
}

pub fn read_item_id(file: &impl AsFd) -> io::Result<Option<String>> {
    read_xattr(file, XATTR_ITEM_ID)
}

pub fn write_item_id(file: &File, item_id: &str) -> io::Result<()> {
    set_xattr(file, XATTR_ITEM_ID, item_id.as_bytes())
}

/// Takes the item id off `file`, which keeps its other attributes; one
/// that carries none is left as it is.
pub fn remove_item_id(file: &File) -> io::Result<()> {
    remove_xattr(file, XATTR_ITEM_ID)
}

pub fn read_ctag(file: &impl AsFd) -> io::Result<Option<String>> {
    read_xattr(file, XATTR_CTAG)
}

pub fn write_ctag(file: &File, ctag: &str) -> io::Result<()> {
    set_xattr(file, XATTR_CTAG, ctag.as_bytes())
}

/// Builds the placeholder nameless, then links it in, so no one can open a
/// half-built one. A zero-size file is created `Hydrated`: nothing to fetch.
pub fn create_placeholder(
    dir: &File,
    name: &str,
    item_id: &str,
    size: u64,
    mtime: SystemTime,
) -> io::Result<()> {
    let raw = nix::fcntl::openat(
        dir.as_fd(),
        ".",
        OFlag::O_TMPFILE | OFlag::O_RDWR | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )?;
    let file = File::from(raw);

    file.set_len(size)?;
    write_item_id(&file, item_id)?;
    write_state(
        &file,
        if size == 0 { State::Hydrated } else { State::OnlineOnly },
    )?;
    set_mtime(&file, mtime)?;

    nix::unistd::linkat(
        file.as_fd(),
        "",
        dir.as_fd(),
        name,
        AtFlags::AT_EMPTY_PATH,
    )?;
    Ok(())
}

/// Sets the file's times to `mtime`, which may be before 1970: `futimens` takes a negative
/// `tv_sec`, with `tv_nsec` counting forward from it.
pub fn set_mtime(file: &File, mtime: SystemTime) -> io::Result<()> {
    let too_far = |_| io::Error::new(io::ErrorKind::InvalidInput, "mtime out of range");
    let (seconds, nanos) = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => (i64::try_from(after.as_secs()).map_err(too_far)?, after.subsec_nanos()),
        Err(before) => {
            let before = before.duration();
            let seconds = -i64::try_from(before.as_secs()).map_err(too_far)?;
            match before.subsec_nanos() {
                0 => (seconds, 0),
                nanos => (seconds - 1, 1_000_000_000 - nanos),
            }
        }
    };
    let spec = nix::sys::time::TimeSpec::new(seconds, nanos as i64);
    nix::sys::stat::futimens(file.as_fd(), &spec, &spec)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

impl Stamp {
    pub fn of(file: &File) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok(Self {
            size: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }

    fn encode(self) -> String {
        format!("{} {}.{}", self.size, self.mtime_sec, self.mtime_nsec)
    }

    /// The attribute's value as [`read_stamp`] parses it, for a caller that
    /// read it by name without opening the file.
    pub fn decode(value: &str) -> Option<Self> {
        let (size, time) = value.split_once(' ')?;
        let (sec, nsec) = time.split_once('.')?;
        Some(Self {
            size: size.parse().ok()?,
            mtime_sec: sec.parse().ok()?,
            mtime_nsec: nsec.parse().ok()?,
        })
    }
}

/// How far an interrupted download got, durably: the cTag of the
/// version it was downloading and the bytes on disk that are known good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub ctag: String,
    pub bytes: u64,
}

impl Progress {
    fn encode(&self) -> String {
        format!("{} {}", self.ctag, self.bytes)
    }

    /// The count is the last word; a cTag may itself contain spaces.
    fn decode(value: &str) -> Option<Self> {
        let (ctag, bytes) = value.rsplit_once(' ')?;
        if ctag.is_empty() {
            return None;
        }
        Some(Self { ctag: ctag.to_owned(), bytes: bytes.parse().ok()? })
    }
}

/// `None` for an absent attribute and for one that does not parse — a
/// checkpoint that cannot be read is no checkpoint, and the download starts
/// from the beginning.
pub fn read_progress(file: &File) -> io::Result<Option<Progress>> {
    Ok(read_xattr(file, XATTR_PROGRESS)?.as_deref().and_then(Progress::decode))
}

pub fn write_progress(file: &File, progress: &Progress) -> io::Result<()> {
    set_xattr(file, XATTR_PROGRESS, progress.encode().as_bytes())
}

pub fn remove_progress(file: &File) -> io::Result<()> {
    remove_xattr(file, XATTR_PROGRESS)
}

/// Whether the file or folder carries its own pin ([`XATTR_PIN`]). Any value
/// counts: the attribute's presence is the pin.
pub fn read_pin(file: &File) -> io::Result<bool> {
    Ok(read_xattr(file, XATTR_PIN)?.is_some())
}

/// Pins the file or folder. Under the read-only lock, the owner's write bit
/// is lifted for the moment of the write, as for every other attribute.
pub fn write_pin(file: &File) -> io::Result<()> {
    set_xattr(file, XATTR_PIN, b"1")
}

/// Takes the pin off; a file or folder with none is left as it is.
pub fn remove_pin(file: &File) -> io::Result<()> {
    remove_xattr(file, XATTR_PIN)
}

pub fn write_stamp(file: &File) -> io::Result<()> {
    set_xattr(file, XATTR_STAMP, Stamp::of(file)?.encode().as_bytes())
}

/// Writes `stamp` rather than the file's own size and time: an upload's
/// snapshot, the content that went up, so that an edit made while it went
/// up still differs from the stamp (`docs/design/writes.md` §5).
pub fn write_given_stamp(file: &File, stamp: Stamp) -> io::Result<()> {
    set_xattr(file, XATTR_STAMP, stamp.encode().as_bytes())
}

pub fn read_stamp(file: &File) -> io::Result<Option<Stamp>> {
    Ok(read_xattr(file, XATTR_STAMP)?.as_deref().and_then(Stamp::decode))
}

pub fn remove_stamp(file: &File) -> io::Result<()> {
    remove_xattr(file, XATTR_STAMP)
}

/// True when the file still looks exactly as it did when hydration finished.
pub fn stamp_matches(file: &File) -> io::Result<bool> {
    Ok(read_stamp(file)? == Some(Stamp::of(file)?))
}

/// Releases every block while keeping the logical size.
///
/// A zero-length file has no blocks to release, and `fallocate` with `len ==
/// 0` returns `EINVAL` unconditionally at the VFS level (not filesystem
/// specific), so that case is short-circuited before the syscall rather than
/// surfaced as an error. Dehydration and startup recovery (§4.4)
/// both call this unconditionally, including on the zero-byte placeholders
/// this crate already treats as a first-class case (see `create_placeholder`).
pub fn punch_all(file: &File) -> io::Result<()> {
    let size = file.metadata()?.len() as i64;
    if size == 0 {
        return Ok(());
    }
    nix::fcntl::fallocate(
        file.as_fd(),
        FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE,
        0,
        size,
    )?;
    Ok(())
}

/// A placeholder as the read phase makes it (see [`create_placeholder_with`]).
pub struct PlaceholderSpec<'a> {
    pub item_id: &'a str,
    pub size: u64,
    pub mtime: SystemTime,
    pub ctag: Option<&'a str>,
    /// [`LOCKED_FILE_MODE`] in a folder that shows OneDrive, [`OPEN_FILE_MODE`] otherwise.
    pub mode: u32,
}

/// [`create_placeholder`] for the read phase: with the cTag of the version it
/// stands for, the lock's mode, and — for an empty file, created `hydrated`
/// since there is nothing to fetch — a stamp, so that an empty file is never
/// taken for one changed locally. Built nameless and linked in last, like
/// every placeholder; the caller holds a write window on `dir` when the
/// folder is locked (`O_TMPFILE` and `linkat` need write permission on it).
pub fn create_placeholder_with(dir: &File, name: &str, spec: &PlaceholderSpec<'_>) -> io::Result<()> {
    let raw = nix::fcntl::openat(
        dir.as_fd(),
        ".",
        OFlag::O_TMPFILE | OFlag::O_RDWR | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(OPEN_FILE_MODE),
    )?;
    let file = File::from(raw);
    file.set_len(spec.size)?;
    write_item_id(&file, spec.item_id)?;
    if let Some(ctag) = spec.ctag {
        write_ctag(&file, ctag)?;
    }
    write_state(&file, if spec.size == 0 { State::Hydrated } else { State::OnlineOnly })?;
    set_mtime(&file, spec.mtime)?;
    if spec.size == 0 {
        write_stamp(&file)?;
    }
    set_mode(&file, spec.mode)?;
    nix::unistd::linkat(file.as_fd(), "", dir.as_fd(), name, AtFlags::AT_EMPTY_PATH)?;
    Ok(())
}

/// A directory for a OneDrive folder, labelled before anything can be put in
/// it: made under `temp_name`, given its item id, and returned open. The
/// caller has it marked (invariant M1) and then renames it to its real name,
/// so the real name never shows a folder without its id. A crash after the
/// label leaves a `temp_name` the next reconcile recognises by that id; one
/// before it — or a label the disk had no room for — leaves one with no id,
/// which the caller clears when it next needs the name (the daemon's
/// `Materializer::labelled_dir`).
pub fn create_dir_item(parent: &File, temp_name: &str, item_id: &str) -> io::Result<File> {
    nix::sys::stat::mkdirat(parent.as_fd(), temp_name, Mode::from_bits_truncate(OPEN_DIR_MODE))?;
    let fd = nix::fcntl::openat(
        parent.as_fd(),
        temp_name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let dir = File::from(fd);
    set_mode(&dir, OPEN_DIR_MODE)?;
    write_item_id(&dir, item_id)?;
    Ok(dir)
}

/// A writable descriptor for the inode `file` is open on — the same inode,
/// whatever its name is by now — lifting the owner's write permission for the
/// moment of the open when the lock has taken it away.
///
/// The reopen goes through `/proc/self/fd/<n>`, which resolves to the open
/// inode itself rather than to a name, so nothing can be swapped in between.
/// Drop `file` afterwards: a second descriptor of one's own makes a write
/// lease on the file impossible to take.
pub fn reopen_writable(file: &File) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let flags = OFlag::from_bits_truncate(nix::fcntl::fcntl(file.as_fd(), nix::fcntl::FcntlArg::F_GETFL)?);
    if flags & OFlag::O_ACCMODE == OFlag::O_RDWR {
        return file.try_clone();
    }
    let path = format!("/proc/self/fd/{}", file.as_raw_fd());
    with_owner_write(file, || {
        std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(&path)
    })
}

/// Releases every block from `offset` to the end, keeping the size — the part
/// of an interrupted download past its checkpoint.
pub fn punch_from(file: &File, offset: u64) -> io::Result<()> {
    let size = file.metadata()?.len();
    if offset >= size {
        return Ok(());
    }
    nix::fcntl::fallocate(
        file.as_fd(),
        FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE,
        offset as i64,
        (size - offset) as i64,
    )?;
    Ok(())
}

/// Removes every `user.konedrive.*` attribute: a file that leaves the folder
/// for the rescue directory is no longer konedrive's.
pub fn strip_konedrive_xattrs(file: &File) -> io::Result<()> {
    let ours: Vec<_> = file
        .list_xattr()?
        .filter(|name| name.as_encoded_bytes().starts_with(b"user.konedrive."))
        .collect();
    for name in ours {
        with_owner_write(file, || match file.remove_xattr(&name) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
            Err(e) => Err(e),
        })?;
    }
    Ok(())
}

/// Takes konedrive's attributes off `file` for good: the item id first, then
/// `fsync`, then the rest. With the id gone the object is an ordinary one (a
/// state with no id is), whatever a crash leaves of the rest; an id with no
/// state, the one combination the helper refuses, is never on disk.
///
/// The rest is not waited for: a crash may leave some of it on an object
/// that is the user's own by then, which nothing reads without an id. So
/// this is only for an object whose content is there (a directory, or a
/// downloaded file): a state that says "not downloaded" left behind with no
/// id is not an ordinary file to the helper.
///
/// Only what is there is touched: an object with no item id gets neither
/// the removal (and its change of mode) nor the `fsync`.
pub fn strip(file: &File) -> io::Result<()> {
    if file.get_xattr(XATTR_ITEM_ID)?.is_some() {
        remove_xattr(file, XATTR_ITEM_ID)?;
        file.sync_all()?;
    }
    strip_konedrive_xattrs(file)
}

#[cfg(test)]
mod tests;
