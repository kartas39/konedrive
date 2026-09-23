//! Privileged check: the ignore mark the helper actually ships really lands.
//!
//! This exists because `fanotify_mark` **lies** about this particular call.
//! `FAN_MARK_ADD | FAN_MARK_IGNORE` returns 0 and creates nothing at all
//! whenever anybody holds the inode open for writing, which for this helper is
//! always: the event fd is `O_RDWR` and the daemon holds an `SCM_RIGHTS` copy
//! of it while it fills the file. A check that asserted on the return value
//! would have passed against the broken code, which is how the defect survived
//! the original implementation and its tests.
//!
//! So every assertion here is on `/proc/self/fdinfo/<group>`, where a mark is
//! either listed or it is not, plus the behaviour that matters: does the next
//! open of the file raise an event or not.
//!
//! It links `konedrive_helper::marks` rather than re-issuing the syscalls, so
//! what is being tested is the code that ships.
//!
//! Run with: `tests/vm/run.sh tests/vm/target/release/vm-ignore-mark`

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_helper::marks::Marks;
use nix::sys::fanotify::{FanotifyEvent, MaskFlags};

const FILESYSTEMS: [&str; 3] = ["btrfs", "ext4", "xfs"];

struct Checks {
    failed: Vec<String>,
}

impl Checks {
    fn check(&mut self, ok: bool, what: &str) {
        if ok {
            println!("  ok    {what}");
        } else {
            println!("  FAIL  {what}");
            self.failed.push(what.to_owned());
        }
    }
}

/// Every mark the group holds, as `/proc/self/fdinfo` reports it. This is the
/// only honest source of truth for whether a mark exists.
fn mark_lines(marks: &Marks) -> Vec<String> {
    let fd = marks.group().as_fd().as_raw_fd();
    std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("fanotify ino:"))
        .map(str::to_owned)
        .collect()
}

fn ignore_mark_present(marks: &Marks, ino: u64) -> bool {
    mark_lines(marks)
        .iter()
        .any(|line| line.contains(&format!("ino:{ino:x} ")) && line.contains("ignored_mask:10000"))
}

fn next_event(marks: &Marks, within: Duration) -> Option<FanotifyEvent> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match marks.group().read_events() {
            Ok(events) => {
                if let Some(event) = events.into_iter().next() {
                    return Some(event);
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => panic!("read_events: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Takes the event's fd without letting `FanotifyEvent::drop` close it — the
/// same trick, and for the same reason, as the helper's own `take_fd`.
fn take_fd(event: FanotifyEvent) -> OwnedFd {
    use std::os::fd::FromRawFd;
    let raw = event.fd().expect("a permission event carries an fd").as_raw_fd();
    std::mem::forget(event);
    unsafe { OwnedFd::from_raw_fd(raw) }
}

struct Opener {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Opener {
    fn start(path: String) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            let _ = File::open(&path);
            flag.store(true, Ordering::SeqCst);
        });
        Self { done, handle: Some(handle) }
    }

    fn finished(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.done.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

/// Opens the file once more and reports whether the open was suppressed — no
/// event at all. Always leaves the opener unblocked.
fn open_is_suppressed(marks: &Marks, path: &str) -> bool {
    let opener = Opener::start(path.to_owned());
    let suppressed = match next_event(marks, Duration::from_secs(2)) {
        None => true,
        Some(event) => {
            let fd = take_fd(event);
            marks.allow(fd.as_fd()).unwrap();
            false
        }
    };
    assert!(opener.finished(Duration::from_secs(5)), "the opener never unblocked");
    opener.join();
    suppressed
}

fn fresh(dir: &str, name: &str) -> io::Result<(String, u64)> {
    let path = format!("{dir}/{name}");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, b"content")?;
    let ino = std::fs::metadata(&path)?.ino();
    Ok((path, ino))
}

fn main() {
    // A guest that hangs reports nothing at all; bail out loudly instead.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(240));
        eprintln!("WATCHDOG: no progress for 240 s");
        std::process::exit(99);
    });

    let mut checks = Checks { failed: Vec::new() };
    for fs in FILESYSTEMS {
        let dir = format!("/mnt/{fs}/ignore-mark");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the filesystem must be mounted");
        println!("== {fs} ==");

        // The scenario the design turns on, in the order the helper does it:
        // an open is intercepted, the helper places the ignore mark through
        // the event fd, then answers. The event fd is still open at the moment
        // the mark is placed, and a second descriptor for the same open file
        // description is held throughout — that is what the daemon has while
        // it hydrates, and it is what made the original code silently do
        // nothing.
        {
            let (path, ino) = fresh(&dir, "hydrated").unwrap();
            let marks = Marks::new().unwrap();
            let root = File::open(&dir).unwrap();
            marks.mark_dir(root.as_fd()).unwrap();

            let opener = Opener::start(path.clone());
            let event = next_event(&marks, Duration::from_secs(5)).expect("the open was not seen");
            checks.check(
                event.mask().contains(MaskFlags::FAN_OPEN_PERM),
                &format!("[{fs}] a directory mark intercepts an open of a file inside it"),
            );
            let fd = take_fd(event);
            let daemon_copy = fd.try_clone().expect("the daemon's SCM_RIGHTS copy");

            marks.ignore_file(fd.as_fd()).expect("ignore_file must report its own failures");
            checks.check(
                ignore_mark_present(&marks, ino),
                &format!("[{fs}] the ignore mark exists while the event fd is still open"),
            );

            marks.allow(fd.as_fd()).unwrap();
            assert!(opener.finished(Duration::from_secs(5)));
            opener.join();
            drop(fd);
            checks.check(
                ignore_mark_present(&marks, ino),
                &format!("[{fs}] it is still there once the event has been answered"),
            );
            drop(daemon_copy);

            checks.check(
                open_is_suppressed(&marks, &path),
                &format!("[{fs}] and the next open of that file raises no event"),
            );

            // Writing to a hydrated file must not send its next open back to
            // us: an ignored mask without FAN_MARK_IGNORED_SURV_MODIFY is
            // cleared by the kernel on every modification.
            std::fs::OpenOptions::new().append(true).open(&path).unwrap().set_len(8).unwrap();
            checks.check(
                ignore_mark_present(&marks, ino),
                &format!("[{fs}] the mark survives the file being modified"),
            );
            checks.check(
                open_is_suppressed(&marks, &path),
                &format!("[{fs}] and the open after a modification is still suppressed"),
            );

            // ClearIgnore, the way dehydration uses it: through an ordinary
            // descriptor the daemon opened and passed over.
            let handle = File::options().read(true).write(true).open(&path).unwrap();
            marks.clear_ignore(handle.as_fd()).expect("clear_ignore must succeed");
            checks.check(
                !ignore_mark_present(&marks, ino),
                &format!("[{fs}] clear_ignore removes it again"),
            );
            checks.check(
                !open_is_suppressed(&marks, &path),
                &format!("[{fs}] so the file is intercepted once more"),
            );
        }

        // An evictable mark is designed to vanish, so the helper meets this
        // every day: clearing a mark that is not there must not be an error,
        // or a routine dehydration tears down the daemon connection.
        {
            let (path, _) = fresh(&dir, "never-marked").unwrap();
            let marks = Marks::new().unwrap();
            let handle = File::options().read(true).write(true).open(&path).unwrap();
            checks.check(
                marks.clear_ignore(handle.as_fd()).is_ok(),
                &format!("[{fs}] clearing an ignore mark that was never placed is not an error"),
            );
            let root = File::open(&dir).unwrap();
            checks.check(
                marks.unmark_dir(root.as_fd()).is_ok(),
                &format!("[{fs}] nor is unmarking a directory that was never marked"),
            );
        }

        // The startup walk: every directory under the root, and nothing
        // outside it. A symlink pointing out of the tree must not be followed
        // — that is how a user would otherwise have the helper, running as
        // root, mark every directory on the machine.
        {
            let tree = format!("{dir}/tree");
            std::fs::create_dir_all(format!("{tree}/a/b/c")).unwrap();
            std::fs::create_dir_all(format!("{tree}/d")).unwrap();
            std::fs::write(format!("{tree}/a/file"), b"x").unwrap();
            std::os::unix::fs::symlink("/etc", format!("{tree}/escape")).unwrap();

            let marks = Marks::new().unwrap();
            let root = File::open(&tree).unwrap();
            let report = konedrive_helper::marks::walk_and_mark(&marks, root.as_fd(), &tree);
            // The root itself plus a, a/b, a/b/c and d. Not `escape`, which is
            // a symlink, and not `a/file`, which is a file.
            checks.check(
                report.marked == 5,
                &format!(
                    "[{fs}] the walk marks the root and the 4 directories under it (got {})",
                    report.marked
                ),
            );
            checks.check(
                !report.degraded(),
                &format!("[{fs}] with no failures ({:?})", report.failures),
            );
            let etc_ino = std::fs::metadata("/etc").unwrap().ino();
            checks.check(
                !mark_lines(&marks).iter().any(|l| l.contains(&format!("ino:{etc_ino:x} "))),
                &format!("[{fs}] and never follows a symlink out of the tree"),
            );
        }
    }

    println!();
    if checks.failed.is_empty() {
        println!("all checks passed");
    } else {
        println!("{} check(s) FAILED:", checks.failed.len());
        for failure in &checks.failed {
            println!("  {failure}");
        }
        std::process::exit(1);
    }
}
