//! A placeholder is an ordinary sparse file whose state lives in its xattrs.

use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::str::FromStr;
use std::time::SystemTime;

use nix::fcntl::{AtFlags, FallocateFlags, OFlag};
use nix::sys::stat::Mode;
use xattr::FileExt;

pub const XATTR_ITEM_ID: &str = "user.konedrive.item-id";
pub const XATTR_STATE: &str = "user.konedrive.state";
pub const XATTR_STAMP: &str = "user.konedrive.stamp";
pub const XATTR_ROOT: &str = "user.konedrive.root";
pub const XATTR_CTAG: &str = "user.konedrive.ctag";
pub const XATTR_PROGRESS: &str = "user.konedrive.progress";

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

fn read_xattr(file: &File, name: &str) -> io::Result<Option<String>> {
    // `xattr::FileExt::get_xattr` already turns a missing attribute
    // (ENODATA/ENOATTR) into `Ok(None)` internally (see the `xattr` crate's
    // `extract_noattr`), so there is no ENODATA `Err` case left to match
    // here.
    match file.get_xattr(name) {
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
pub fn read_state(file: &File) -> Result<Option<State>, StateError> {
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

pub fn read_item_id(file: &File) -> io::Result<Option<String>> {
    read_xattr(file, XATTR_ITEM_ID)
}

pub fn write_item_id(file: &File, item_id: &str) -> io::Result<()> {
    set_xattr(file, XATTR_ITEM_ID, item_id.as_bytes())
}

pub fn read_ctag(file: &File) -> io::Result<Option<String>> {
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

pub fn set_mtime(file: &File, mtime: SystemTime) -> io::Result<()> {
    let since_epoch = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mtime before the epoch"))?;
    let spec = nix::sys::time::TimeSpec::new(
        since_epoch.as_secs() as i64,
        since_epoch.subsec_nanos() as i64,
    );
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

    fn decode(value: &str) -> Option<Self> {
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

pub fn write_stamp(file: &File) -> io::Result<()> {
    set_xattr(file, XATTR_STAMP, Stamp::of(file)?.encode().as_bytes())
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

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        create_placeholder, punch_all, read_item_id, read_state, stamp_matches, write_stamp,
        write_state, remove_stamp, State, StateError, XATTR_STATE, with_owner_write,
        XATTR_PROGRESS, Progress, write_progress, read_progress, remove_progress, write_ctag,
        read_ctag, LOCKED_FILE_MODE,
        PlaceholderSpec, create_placeholder_with, create_dir_item, reopen_writable, punch_from,
        strip_konedrive_xattrs, write_item_id, combine_op_and_restore_results,
    };

    fn dir() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let handle = File::open(dir.path()).unwrap();
        (dir, handle)
    }

    #[test]
    fn creates_a_sparse_placeholder_with_the_real_size() {
        let (dir, handle) = dir();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        create_placeholder(&handle, "movie.mkv", "ITEM1", 4_700_000_000, mtime).unwrap();

        let path = dir.path().join("movie.mkv");
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 4_700_000_000);
        assert!(meta.blocks() < 64, "expected a sparse file, got {} blocks", meta.blocks());
        assert_eq!(meta.modified().unwrap(), mtime);

        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_item_id(&file).unwrap().as_deref(), Some("ITEM1"));
    }

    #[test]
    fn zero_byte_files_are_created_hydrated() {
        let (dir, handle) = dir();
        create_placeholder(&handle, "empty.txt", "ITEM2", 0, SystemTime::now()).unwrap();
        let file = File::open(dir.path().join("empty.txt")).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    }

    #[test]
    fn a_half_built_placeholder_is_never_visible() {
        // linkat is the last step, so the name appears only once everything is set.
        let (dir, handle) = dir();
        create_placeholder(&handle, "doc.pdf", "ITEM3", 1024, SystemTime::now()).unwrap();
        let file = File::open(dir.path().join("doc.pdf")).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_item_id(&file).unwrap().as_deref(), Some("ITEM3"));
        assert_eq!(file.metadata().unwrap().len(), 1024);
    }

    #[test]
    fn state_round_trips_and_unmanaged_files_have_none() {
        let (dir, _handle) = dir();
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"x").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), None);
        write_state(&file, State::Hydrating).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrating));
        write_state(&file, State::Hydrated).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
    }

    /// The distinction the helper's "never allow zeros" rule turns on: a file
    /// with no state attribute at all is not ours and is none of our business,
    /// but a file whose state attribute we cannot make sense of is a managed
    /// file in an unknown condition, and the two must not report the same way.
    #[test]
    fn an_unparseable_state_is_an_error_not_an_absent_one() {
        let (dir, _handle) = dir();
        let path = dir.path().join("corrupt.bin");
        std::fs::write(&path, b"x").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), None, "no attribute means no attribute");

        for bad in ["", "hydrate", "HYDRATED", "online only", "\u{fffd}"] {
            xattr::FileExt::set_xattr(&file, XATTR_STATE, bad.as_bytes()).unwrap();
            match read_state(&file) {
                Err(StateError::Corrupt(value)) => assert_eq!(value, bad),
                other => panic!("{bad:?} must not be readable as a state: {other:?}"),
            }
        }
    }

    #[test]
    fn stamp_detects_local_modification() {
        let (dir, _handle) = dir();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"12345").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        write_stamp(&file).unwrap();
        assert!(stamp_matches(&file).unwrap());

        let mut appended = File::options().append(true).open(&path).unwrap();
        appended.write_all(b"6").unwrap();
        drop(appended);
        let file = File::open(&path).unwrap();
        assert!(!stamp_matches(&file).unwrap(), "a changed file must not match its stamp");
    }

    #[test]
    fn punch_all_frees_blocks_and_keeps_the_size() {
        let (dir, _handle) = dir();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        punch_all(&file).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1 << 20, "size must be preserved");
        assert!(meta.blocks() < 64, "expected the blocks to be freed, got {}", meta.blocks());
        let mut content = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut content).unwrap();
        assert!(content.iter().all(|b| *b == 0));
    }

    #[test]
    fn punch_all_on_a_zero_length_file_is_a_no_op() {
        // `fallocate(..., len=0, ...)` always returns EINVAL; dehydration and
        // startup recovery call `punch_all` unconditionally, including on
        // the zero-byte placeholders this crate treats as already hydrated.
        let (dir, _handle) = dir();
        let path = dir.path().join("empty.bin");
        let file =
            File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        punch_all(&file).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    fn locked(dir: &std::path::Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        path
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn combine_op_and_restore_results_all_four_cases() {
        const MODE: u32 = 0o444;

        // Case 1: op ok, restore ok -> return op value
        let result = combine_op_and_restore_results::<i32>(Ok(42), Ok(()), MODE);
        assert!(matches!(result, Ok(42)), "case 1: op ok, restore ok should return op value");

        // Case 2: op ok, restore err -> return error mentioning write succeeded
        let restore_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fchmod failed");
        let result: std::io::Result<i32> = combine_op_and_restore_results(Ok(42), Err(restore_err), MODE);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("write succeeded"), "error should mention write succeeded: {err_msg}");
        assert!(err_msg.contains("0o444") || err_msg.contains("444"), "error should mention mode: {err_msg}");

        // Case 3: op err, restore ok -> return op error
        let op_err = std::io::Error::new(std::io::ErrorKind::Other, "setfattr failed");
        let result: std::io::Result<i32> = combine_op_and_restore_results(Err(op_err), Ok(()), MODE);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("setfattr failed"), "error should contain op error: {err_msg}");
        assert!(!err_msg.contains("mode could not be put back"), "should not mention restore in op-only error: {err_msg}");

        // Case 4: op err, restore err -> return error carrying both
        let op_err = std::io::Error::new(std::io::ErrorKind::Other, "setfattr failed");
        let restore_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fchmod failed");
        let result: std::io::Result<i32> = combine_op_and_restore_results(Err(op_err), Err(restore_err), MODE);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("setfattr failed"), "error should contain op error: {err_msg}");
        assert!(err_msg.contains("mode could not be put back"), "error should mention restore failure: {err_msg}");
        assert!(err_msg.contains("0o444") || err_msg.contains("444"), "error should mention mode: {err_msg}");
    }

    /// Measured on Btrfs: the owner's `setfattr` on a
    /// `0444` file fails `EACCES`. Everything the daemon writes as an
    /// attribute must still land, and the file must stay `0444`.
    #[test]
    fn attribute_writes_work_on_a_locked_file_and_leave_it_locked() {
        let (dir, _handle) = dir();
        let path = locked(dir.path(), "f.bin", b"12345");
        let file = File::open(&path).unwrap();
        assert!(
            xattr::FileExt::set_xattr(&file, "user.probe", b"x").is_err(),
            "the premise: a plain attribute write on a 0444 file is refused"
        );
        write_state(&file, State::Hydrated).unwrap();
        write_stamp(&file).unwrap();
        write_ctag(&file, "c1").unwrap();
        write_progress(&file, &Progress { ctag: "c1".into(), bytes: 3 }).unwrap();
        remove_progress(&file).unwrap();
        remove_stamp(&file).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
        assert_eq!(read_progress(&file).unwrap(), None);
        assert_eq!(mode_of(&path), 0o444);
    }

    #[test]
    fn the_mode_is_put_back_even_when_the_write_fails() {
        let (dir, _handle) = dir();
        let path = locked(dir.path(), "f.bin", b"x");
        let file = File::open(&path).unwrap();
        let result: std::io::Result<()> = with_owner_write(&file, || Err(std::io::Error::other("refused")));
        assert!(result.is_err());
        assert_eq!(mode_of(&path), 0o444);
    }

    #[test]
    fn progress_round_trips_and_nonsense_reads_as_none() {
        let (dir, _handle) = dir();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"x").unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        let progress = Progress { ctag: "\"{A B},2\"".into(), bytes: 16 << 20 };
        write_progress(&file, &progress).unwrap();
        assert_eq!(read_progress(&file).unwrap(), Some(progress), "a cTag may contain a space");
        for bad in ["", "123", "c1 x", " 5"] {
            xattr::FileExt::set_xattr(&file, XATTR_PROGRESS, bad.as_bytes()).unwrap();
            assert_eq!(read_progress(&file).unwrap(), None, "{bad:?}");
        }
    }

    #[test]
    fn a_read_phase_placeholder_carries_its_ctag_and_the_locks_mode() {
        let (dir, handle) = dir();
        let spec = PlaceholderSpec { item_id: "I1", size: 1 << 20, mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000), ctag: Some("c1"), mode: LOCKED_FILE_MODE };
        create_placeholder_with(&handle, "a.bin", &spec).unwrap();
        let path = dir.path().join("a.bin");
        let file = File::open(&path).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(read_item_id(&file).unwrap().as_deref(), Some("I1"));
        assert_eq!(read_ctag(&file).unwrap().as_deref(), Some("c1"));
        assert_eq!(mode_of(&path), 0o444);
        assert_eq!(file.metadata().unwrap().modified().unwrap(), spec.mtime);
    }

    /// An empty file is created `hydrated` — nothing to fetch — and, unlike
    /// part 1's placeholders, stamped: a `hydrated` file without a stamp is
    /// what `Hydrate()` refills (H109) and what a reconcile would take for a
    /// file changed locally.
    #[test]
    fn an_empty_read_phase_placeholder_is_hydrated_and_stamped() {
        let (dir, handle) = dir();
        let spec = PlaceholderSpec { item_id: "I2", size: 0, mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000), ctag: Some("c2"), mode: LOCKED_FILE_MODE };
        create_placeholder_with(&handle, "empty", &spec).unwrap();
        let file = File::open(dir.path().join("empty")).unwrap();
        assert_eq!(read_state(&file).unwrap(), Some(State::Hydrated));
        assert!(stamp_matches(&file).unwrap());
    }

    #[test]
    fn a_folder_is_labelled_before_it_has_its_real_name() {
        let (dir, handle) = dir();
        let made = create_dir_item(&handle, ".konedrive-new-D1", "D1").unwrap();
        assert_eq!(read_item_id(&made).unwrap().as_deref(), Some("D1"));
        assert!(dir.path().join(".konedrive-new-D1").is_dir());
        assert_eq!(mode_of(&dir.path().join(".konedrive-new-D1")), 0o755);
    }

    #[test]
    fn a_locked_file_is_reopened_writable_on_the_same_inode() {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::{FileExt, MetadataExt};
        let (dir, _handle) = dir();
        let path = locked(dir.path(), "f.bin", b"hello");
        let read_only = File::open(&path).unwrap();
        let writable = reopen_writable(&read_only).unwrap();
        let (a, b) = (read_only.metadata().unwrap(), writable.metadata().unwrap());
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
        writable.write_all_at(b"J", 0).unwrap();
        let mut back = String::new();
        let mut reader = File::open(&path).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_to_string(&mut back).unwrap();
        assert_eq!(back, "Jello");
        assert_eq!(mode_of(&path), 0o444);
    }

    #[test]
    fn punching_from_an_offset_keeps_what_is_before_it() {
        let (dir, _handle) = dir();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
        let file = File::options().read(true).write(true).open(&path).unwrap();
        punch_from(&file, 256 << 10).unwrap();
        let mut content = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut content).unwrap();
        assert_eq!(content.len(), 1 << 20);
        assert!(content[..256 << 10].iter().all(|b| *b == 7));
        assert!(content[256 << 10..].iter().all(|b| *b == 0));
        punch_from(&file, 2 << 20).unwrap(); // past the end: nothing to do
    }

    #[test]
    fn stripping_leaves_no_konedrive_attribute_and_keeps_the_others() {
        let (dir, _handle) = dir();
        let path = locked(dir.path(), "f.bin", b"x");
        let file = File::open(&path).unwrap();
        write_item_id(&file, "I").unwrap();
        write_state(&file, State::Hydrated).unwrap();
        with_owner_write(&file, || xattr::FileExt::set_xattr(&file, "user.other", b"keep")).unwrap();
        strip_konedrive_xattrs(&file).unwrap();
        let names: Vec<_> = xattr::FileExt::list_xattr(&file).unwrap().collect();
        // Check that no konedrive attributes remain and user.other is preserved
        for name in &names {
            assert!(!name.as_encoded_bytes().starts_with(b"user.konedrive."), "konedrive attribute not stripped: {name:?}");
        }
        assert!(names.iter().any(|n| n == std::ffi::OsStr::new("user.other")), "user.other attribute was not preserved");
    }
}
