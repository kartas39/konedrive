//! `vm-scenarios --unit <helper pid> <base> <name>`: the daemon's side of
//! `tests/vm/run.sh unit`, run against a helper that **systemd** started from
//! the shipped `packaging/systemd/konedrive-helper.service`, not one this
//! programme spawned. `tests/vm/helper_unit_test.sh` boots the guest with
//! systemd as init, starts the unit, and hands over its `MainPID`.
//!
//! Everything else in this suite runs the helper as plain root with every
//! capability. Under the unit it has two capabilities, a read-only view of
//! the whole filesystem but two directories, no network, and a seccomp
//! filter — so this asks the questions a sandbox could break:
//!
//! - a daemon of an ordinary uid connects to the unit's socket and is greeted;
//! - `RegisterRoot` of a new folder `<base>/<name>` is accepted, and the
//!   folder is marked;
//! - `MarkDir` marks a `0700` subdirectory of the user's (only
//!   `CAP_DAC_READ_SEARCH` lets the helper into it);
//! - an open of a `0600` placeholder from another process is suspended,
//!   reaches this daemon as a `HydrateRequest`, and the reader gets the real
//!   content; the helper then puts its ignore mark on the file, and the next
//!   open raises no request;
//! - every folder an earlier run registered in `<base>` — before systemd
//!   restarted the helper — was marked again by the helper's startup walk,
//!   and a placeholder placed in it now is intercepted too.
//!
//! The marks are read from `/proc/<helper>/fdinfo`, as in the rest of the
//! suite. That needs root, while the connection and the files must belong to
//! the user, so this process runs with the user's *effective* ids and keeps
//! root as its saved uid: `SO_PEERCRED` reports the effective uid, and files
//! are created with it. [`as_root`] switches back for the reads of `fdinfo`,
//! at moments when no fill is running.

use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::placeholder::{create_placeholder, read_state, State};
use konedrive_proto::SOCKET_PATH;
use konedrived::sync::helper::HelperLink;
use konedrived::sync::root;
use konedrived::sync::source::ContentSource;
use konedrived::sync::{serve_hydrations, InodeLocks};

use crate::{dir_mark_present, ignore_mark_present, statfs_type, Checks, Reader, TestSource};

/// The uid (and gid) the folders, their files and the daemon connection
/// belong to. Nothing in the guest needs to know it by name.
const USER: u32 = 1000;

const BTRFS_MAGIC: i64 = 0x9123_683E;

/// What every row of this mode is labelled with, where the suite puts a
/// filesystem name.
const LABEL: &str = "unit";

pub(crate) fn unit_mode(helper_pid: u32, base: &Path, name: &str) -> i32 {
    let mut checks = Checks::default();
    if let Err(why) = run(helper_pid, base, name, &mut checks) {
        checks.record(LABEL, "the check ran to the end", Err(why));
    }
    // Whatever happened above, root again.
    set_effective(0, 0);
    println!();
    println!("{} passed, {} failed", checks.passed, checks.failed.len());
    for failure in &checks.failed {
        println!("  {failure}");
    }
    if checks.failed.is_empty() {
        0
    } else {
        1
    }
}

fn run(helper_pid: u32, base: &Path, name: &str, checks: &mut Checks) -> Result<(), String> {
    let found = statfs_type(Path::new("/mnt/btrfs"))?;
    if found != BTRFS_MAGIC {
        return Err(format!("/mnt/btrfs reports f_type {found:#x}, not btrfs"));
    }
    let source_dir = base.join("source");
    let folder = base.join(name);

    // What an earlier run registered, before the helper was restarted.
    let mut earlier: Vec<PathBuf> = std::fs::read_dir(base)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    earlier.retain(|path| path.is_dir() && *path != source_dir);
    earlier.sort();
    if earlier.contains(&folder) {
        return Err(format!(
            "{folder:?} is already there: every run needs a name of its own"
        ));
    }

    // Laid out as root, then handed to the user. The folder is empty, as
    // the daemon's own checks require of a folder it registers for the first
    // time.
    for (dir, mode) in [(base, 0o755), (&source_dir, 0o700), (&folder, 0o700)] {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
        if dir != base {
            std::os::unix::fs::chown(dir, Some(USER), Some(USER))
                .map_err(|e| format!("cannot chown {dir:?}: {e}"))?;
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("cannot chmod {dir:?}: {e}"))?;
    }

    // Root's supplementary groups would follow us into the user's identity.
    // SAFETY: a one-element, correctly sized list.
    if unsafe { libc::setgroups(1, &USER) } != 0 {
        return Err(format!("setgroups: {}", std::io::Error::last_os_error()));
    }
    set_effective(USER, USER);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let source = TestSource::new(source_dir);

    // The unit is active, so the socket is there; the retry covers only the
    // moment between `bind` and `listen`.
    let deadline = Instant::now() + Duration::from_secs(20);
    let (link, requests) = loop {
        match runtime.block_on(HelperLink::connect(Path::new(SOCKET_PATH))) {
            Ok(pair) => break pair,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("cannot connect to {SOCKET_PATH}: {e}")),
        }
    };
    checks.record(
        LABEL,
        &format!("uid {USER} connects to the unit's socket and is greeted"),
        Ok(()),
    );
    let serving = runtime.spawn(serve_hydrations(
        link.clone(),
        requests,
        Arc::clone(&source) as Arc<dyn ContentSource>,
        InodeLocks::new(),
    ));

    let ctx = Unit {
        helper_pid,
        runtime: &runtime,
        link: &link,
        source: &source,
        name,
    };
    let outcome = ctx.new_folder(&folder, checks).and_then(|()| {
        earlier
            .iter()
            .try_for_each(|folder| ctx.earlier_folder(folder, checks))
    });

    serving.abort();
    drop(link);
    runtime.shutdown_timeout(Duration::from_secs(5));
    outcome
}

struct Unit<'a> {
    helper_pid: u32,
    runtime: &'a tokio::runtime::Runtime,
    link: &'a HelperLink,
    source: &'a Arc<TestSource>,
    /// This run's name, which keeps its item ids apart from an earlier run's.
    name: &'a str,
}

impl Unit<'_> {
    fn new_folder(&self, folder: &Path, checks: &mut Checks) -> Result<(), String> {
        self.runtime
            .block_on(root::register_root(self.link, folder))
            .map_err(|e| format!("RegisterRoot of {folder:?} was refused: {e}"))?;
        checks.record(
            LABEL,
            "RegisterRoot through the unit's socket is accepted",
            Ok(()),
        );
        let marked = self.marked(&[folder])?;
        checks.record(LABEL, "the registered folder is marked", marked);

        let sub = folder.join("sub");
        std::fs::create_dir(&sub).map_err(|e| format!("cannot create {sub:?}: {e}"))?;
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let dir = File::open(&sub).map_err(|e| format!("cannot open {sub:?}: {e}"))?;
        self.runtime
            .block_on(self.link.mark_dir(&dir))
            .map_err(|e| format!("MarkDir was refused: {e}"))?;
        let marked = self.marked(&[&sub])?;
        checks.record(
            LABEL,
            "MarkDir marks a 0700 subdirectory of the user's",
            marked,
        );

        self.open_fills(folder, "report.pdf", "ITEM1", checks)?;
        self.open_fills(&sub, "notes.txt", "ITEM2", checks)
    }

    fn earlier_folder(&self, folder: &Path, checks: &mut Checks) -> Result<(), String> {
        let label = folder.file_name().unwrap_or_default().to_string_lossy();
        let marked = self.marked(&[folder, &folder.join("sub")])?;
        checks.record(
            LABEL,
            &format!(
                "the startup walk marked {label}, registered before the restart, and its 0700 \
                 subdirectory"
            ),
            marked,
        );
        self.open_fills(&folder.join("sub"), "after-restart.txt", "ITEM3", checks)
    }

    /// Places a `0600` placeholder, opens it from another process, and checks
    /// the whole round trip, the ignore mark, and that a second open is left
    /// alone.
    fn open_fills(
        &self,
        dir: &Path,
        file: &str,
        item: &str,
        checks: &mut Checks,
    ) -> Result<(), String> {
        let item = format!("{}-{item}", self.name);
        let payload = format!("the real content of {} in {}", file, self.name).into_bytes();
        let path = dir.join(file);
        let fetches = self.source.fetches();
        let filled = self
            .place(dir, file, &item, &payload)
            .and_then(|()| read(&path))
            .and_then(|got| {
                if got != payload {
                    return Err(format!(
                        "the reader got {:?}",
                        String::from_utf8_lossy(&got)
                    ));
                }
                if self.source.fetches() != fetches + 1 {
                    return Err(format!(
                        "{} request(s) reached the daemon, not 1",
                        self.source.fetches() - fetches
                    ));
                }
                match state_of(&path)? {
                    Some(State::Hydrated) => Ok(()),
                    other => Err(format!("the state after the fill is {other:?}")),
                }
            });
        let ok = filled.is_ok();
        checks.record(
            LABEL,
            &format!(
                "an open of {file} in another process is suspended, reaches this daemon, and \
                 reads the real content"
            ),
            filled,
        );
        if !ok {
            return Ok(());
        }

        let ino = ino_of(&path)?;
        let ignored = if as_root(|| ignore_mark_present(self.helper_pid, ino)) {
            Ok(())
        } else {
            Err("fdinfo shows no ignore mark on the file".to_owned())
        };
        checks.record(
            LABEL,
            &format!("the helper puts its ignore mark on {file}"),
            ignored,
        );

        let again = read(&path).and_then(|got| {
            if got != payload {
                return Err(format!(
                    "the second reader got {:?}",
                    String::from_utf8_lossy(&got)
                ));
            }
            match self.source.fetches() - fetches {
                1 => Ok(()),
                n => Err(format!("{n} request(s) in all, not 1")),
            }
        });
        checks.record(
            LABEL,
            &format!("a second open of {file} raises no request"),
            again,
        );
        Ok(())
    }

    /// Whether the helper holds an open-permission mark on every directory.
    fn marked(&self, dirs: &[&Path]) -> Result<Result<(), String>, String> {
        let mut missing = Vec::new();
        for dir in dirs {
            let ino = ino_of(dir)?;
            if !as_root(|| dir_mark_present(self.helper_pid, ino)) {
                missing.push(dir.display().to_string());
            }
        }
        Ok(if missing.is_empty() {
            Ok(())
        } else {
            Err(format!("no mark on {}", missing.join(", ")))
        })
    }

    /// A placeholder, `0600` and the user's, whose payload `TestSource` serves.
    fn place(&self, dir: &Path, file: &str, item: &str, payload: &[u8]) -> Result<(), String> {
        std::fs::write(self.source.dir.join(item), payload)
            .map_err(|e| format!("cannot write the payload: {e}"))?;
        let parent = File::open(dir).map_err(|e| format!("cannot open {dir:?}: {e}"))?;
        create_placeholder(
            &parent,
            file,
            item,
            payload.len() as u64,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .map_err(|e| format!("cannot create the placeholder: {e}"))?;
        std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot chmod the placeholder: {e}"))
    }
}

/// The open that is meant to be intercepted, in a child process: this one is
/// the daemon, and the helper lets the daemon's own opens straight through.
fn read(path: &Path) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Reader::start(&exe, path)?.content(Duration::from_secs(60))
}

fn state_of(path: &Path) -> Result<Option<State>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {path:?}: {e}"))?;
    read_state(&file).map_err(|e| format!("cannot read the state of {path:?}: {e}"))
}

fn ino_of(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|m| m.ino())
        .map_err(|e| format!("{path:?}: {e}"))
}

/// Runs `read` with root's effective ids, then goes back to the user's.
fn as_root<T>(read: impl FnOnce() -> T) -> T {
    set_effective(0, 0);
    let out = read();
    set_effective(USER, USER);
    out
}

/// Sets the effective uid and gid, leaving the real and saved ones at root.
/// glibc applies each call to every thread of the process, tokio's included.
fn set_effective(uid: u32, gid: u32) {
    const KEEP: u32 = u32::MAX;
    // SAFETY: plain credential calls. Back to root, the uid goes first, since
    // setting the gid needs root's capabilities back; away from it, last.
    let failed = unsafe {
        if uid == 0 {
            libc::setresuid(KEEP, 0, KEEP) != 0 || libc::setresgid(KEEP, gid, KEEP) != 0
        } else {
            libc::setresgid(KEEP, gid, KEEP) != 0 || libc::setresuid(KEEP, uid, KEEP) != 0
        }
    };
    if failed {
        panic!(
            "cannot switch to uid {uid}: {}",
            std::io::Error::last_os_error()
        );
    }
}
