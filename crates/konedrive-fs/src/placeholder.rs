//! What konedrive keeps on a file or a directory of a folder: its extended attributes
//! (`user.konedrive.*`) and its modes.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::str::FromStr;
use std::time::SystemTime;

use nix::fcntl::FallocateFlags;
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
/// "Always keep on this device": `"1"` on every item a pin holds for, its own or a
/// directory's above it. The mount answers it from the store's pins and writes nothing on
/// disk; the Dolphin plugin reads it by name (`docs/design/pinning.md` §2.2).
pub const XATTR_PIN: &str = "user.konedrive.pin";
/// On an item with a change waiting to go up: `pending`, `uploading` or `blocked`. The
/// mount answers it from the row and the plan and writes nothing on disk; Dolphin's
/// emblem plugin reads it as it reads the state (`docs/design/writes.md` §7).
pub const XATTR_SYNC: &str = "user.konedrive.sync";

/// The modes of a folder's entries while no mount lies over it, without their write bits,
/// and while one does (`docs/design/mount.md` §8.4).
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
/// (measured: the owner's `setfattr` on a `0444` file fails `EACCES`). While no
/// mount lies over the folder every file is `0444`, so every attribute the daemon
/// writes goes through here, and the window is that one call. A mode that cannot be
/// put back is an error even when `op` succeeded: the write bit would silently stay.
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

/// Reads through the descriptor it is given and opens nothing.
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
/// one of ours" — is a data-loss bug, not a simplification: a file whose
/// `user.konedrive.state` had been truncated, half-written or set by hand to
/// something we do not recognise would be taken for the user's own, whole
/// file.
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

pub fn write_ctag(file: &File, ctag: &str) -> io::Result<()> {
    set_xattr(file, XATTR_CTAG, ctag.as_bytes())
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

    /// The attribute's value parsed, for a caller that read it by name
    /// without opening the file.
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

/// Writes `stamp` as the file's `user.konedrive.stamp`, rather than the file's own size
/// and time.
pub fn write_given_stamp(file: &File, stamp: Stamp) -> io::Result<()> {
    set_xattr(file, XATTR_STAMP, stamp.encode().as_bytes())
}

/// Releases every block while keeping the logical size.
///
/// A zero-length file has no blocks to release, and `fallocate` with `len ==
/// 0` returns `EINVAL` unconditionally at the VFS level (not filesystem
/// specific), so that case is short-circuited before the syscall rather than
/// surfaced as an error.
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

#[cfg(test)]
mod tests;
