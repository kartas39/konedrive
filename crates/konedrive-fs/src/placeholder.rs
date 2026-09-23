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
/// decision table (spec §5.2) maps `Ok(None)` to `FAN_ALLOW`, so a placeholder
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
    file.set_xattr(XATTR_STATE, state.as_str().as_bytes())
}

pub fn read_item_id(file: &File) -> io::Result<Option<String>> {
    read_xattr(file, XATTR_ITEM_ID)
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
    file.set_xattr(XATTR_ITEM_ID, item_id.as_bytes())?;
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

fn set_mtime(file: &File, mtime: SystemTime) -> io::Result<()> {
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

pub fn write_stamp(file: &File) -> io::Result<()> {
    file.set_xattr(XATTR_STAMP, Stamp::of(file)?.encode().as_bytes())
}

pub fn read_stamp(file: &File) -> io::Result<Option<Stamp>> {
    Ok(read_xattr(file, XATTR_STAMP)?.as_deref().and_then(Stamp::decode))
}

pub fn remove_stamp(file: &File) -> io::Result<()> {
    match file.remove_xattr(XATTR_STAMP) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => Ok(()),
        Err(e) => Err(e),
    }
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
/// surfaced as an error. Dehydration (spec §8) and startup recovery (§4.4)
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

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        create_placeholder, punch_all, read_item_id, read_state, stamp_matches, write_stamp,
        write_state, State, StateError, XATTR_STATE,
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
}
