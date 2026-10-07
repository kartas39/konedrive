//! Proof of concept for the hydration core's interception strategy.
//!
//! Every check prints `PASS <name>` or `FAIL <name>: <why>`; the process exits
//! non-zero if anything failed. Run inside the virtme-ng VM (tests/vm/run.sh).

use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};

/// Where the runner mounts each filesystem, and the `f_type` its superblock must
/// report. `run.sh` puts a tmpfs over `/mnt`, so a mount that silently failed
/// would leave a perfectly writable directory behind and every check would pass
/// against tmpfs. This also catches the binary being run outside the VM.
const FILESYSTEMS: [(&str, i64); 3] = [
    ("/mnt/btrfs", 0x9123_683E), // BTRFS_SUPER_MAGIC
    ("/mnt/ext4", 0x0000_EF53),  // EXT4_SUPER_MAGIC
    ("/mnt/xfs", 0x5846_5342),   // XFS_SUPER_MAGIC
];

/// Prints a marker and kills the process if a check wedges, so a deadlock shows
/// up as a diagnosis instead of a silent VM timeout. Each check bounds its own
/// waits, so nothing legitimate comes close to this.
fn watchdog(after: Duration) {
    thread::spawn(move || {
        thread::sleep(after);
        println!("WATCHDOG: no progress for {after:?} — the process is wedged");
        std::process::exit(2);
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    watchdog(Duration::from_secs(180));
    if args.len() > 1 && args[1] == "--measure" {
        let count: usize = args
            .get(2)
            .and_then(|a| a.parse().ok())
            .unwrap_or(10_000);
        let root = Path::new("/mnt/btrfs");
        let mut failed = false;
        for outcome in [
            measure(root, count),
            measure_files(root, count),
            measure_ignored_files(root, count),
        ] {
            if let Err(why) = outcome {
                println!("MEASURE-FAIL: {why}");
                failed = true;
            }
        }
        std::process::exit(if failed { 1 } else { 0 });
    }

    let mut failures = 0;
    for (fs_root, magic) in FILESYSTEMS {
        println!("== {fs_root}");
        failures += run_checks(Path::new(fs_root), magic);
    }
    println!("failures={failures}");
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

fn check(name: &str, result: Result<(), String>) -> u32 {
    match result {
        Ok(()) => {
            println!("PASS {name}");
            0
        }
        Err(why) => {
            println!("FAIL {name}: {why}");
            1
        }
    }
}

fn run_checks(root: &Path, magic: i64) -> u32 {
    // Nothing below means anything if the filesystem under test is not the one
    // named, so this is fatal for the whole group rather than one failure.
    if check("filesystem is the one it claims", mounted_as(root, magic)) == 1 {
        println!("    skipping {root:?}: the checks would be measuring the wrong filesystem");
        return 1;
    }
    let mut failures = 0;
    failures += check("dir mark intercepts child open", dir_mark_intercepts(root));
    failures += check("ignore mark suppresses parent event", ignore_mark_suppresses(root));
    failures += check("mark covers files created later", covers_new_files(root));
    failures += check("directory open is not intercepted", dir_open_not_intercepted(root));
    failures += check("deny carries our errno", deny_errno(root));
    failures += check("write through event fd is silent", write_through_event_fd(root));
    // Controls: these two record what the documentation claims about the
    // negative cases, so no claim in it rests on a one-off observation.
    failures += check("plain deny flattens to EPERM", plain_deny_is_eperm(root));
    failures += check("O_PATH fd is rejected for marking", o_path_fd_rejected(root));
    failures
}

/// `statfs` says this really is the filesystem we think we are testing.
fn mounted_as(root: &Path, expected: i64) -> Result<(), String> {
    let got = nix::sys::statfs::statfs(root)
        .map_err(|e| format!("statfs {root:?}: {e}"))?
        .filesystem_type()
        .0 as i64;
    if got == expected {
        Ok(())
    } else {
        Err(format!(
            "{root:?} reports f_type {got:#x}, expected {expected:#x} \
             (tmpfs is {:#x}) — the mount did not happen",
            libc::TMPFS_MAGIC
        ))
    }
}

/// A fanotify group set up exactly as the helper will use it.
fn group() -> Fanotify {
    Fanotify::init(
        InitFlags::FAN_CLASS_PRE_CONTENT
            | InitFlags::FAN_CLOEXEC
            | InitFlags::FAN_UNLIMITED_QUEUE
            | InitFlags::FAN_UNLIMITED_MARKS
            | InitFlags::FAN_NONBLOCK,
        EventFFlags::O_RDWR | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC,
    )
    .expect("fanotify_init (needs CAP_SYS_ADMIN)")
}

fn mark_dir(group: &Fanotify, dir: &Path) -> Result<(), String> {
    mark_dir_with(
        group,
        dir,
        MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
    )
}

/// Marking a directory is safe with a plain open: without `FAN_ONDIR` the open
/// of a directory is never intercepted, so this cannot deadlock against our own
/// marks.
fn mark_dir_with(group: &Fanotify, dir: &Path, mask: MaskFlags) -> Result<(), String> {
    let fd = File::open(dir).map_err(|e| format!("open {dir:?}: {e}"))?;
    group
        .mark(MarkFlags::FAN_MARK_ADD, mask, fd.as_fd(), None::<&Path>)
        .map_err(|e| format!("mark {dir:?}: {e}"))
}

/// Marking a *file* inside a directory we have already marked is not: opening
/// it normally raises a `FAN_OPEN_PERM` event aimed at this very process, which
/// then waits for an answer only it could give — a hard deadlock. `O_PATH`
/// avoids the event but `fanotify_mark` rejects such a descriptor with `EBADF`.
/// What works is naming the file relative to a descriptor for its directory,
/// which `fanotify_mark` resolves without ever opening the file.
fn mark_file(group: &Fanotify, file: &Path, flags: MarkFlags, mask: MaskFlags) -> Result<(), String> {
    let parent = file.parent().ok_or_else(|| format!("{file:?} has no parent"))?;
    let name = file
        .file_name()
        .ok_or_else(|| format!("{file:?} has no file name"))?;
    let dir = File::open(parent).map_err(|e| format!("open {parent:?}: {e}"))?;
    group
        .mark(flags, mask, dir.as_fd(), Some(Path::new(name)))
        .map_err(|e| format!("mark {file:?}: {e}"))
}

fn ignore_file(group: &Fanotify, file: &Path) -> Result<(), String> {
    mark_file(
        group,
        file,
        MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_IGNORE | MarkFlags::FAN_MARK_EVICTABLE,
        MaskFlags::FAN_OPEN_PERM,
    )
}

/// Deny with a specific errno: FAN_DENY plus the errno in the top byte.
fn deny_with(errno: i32) -> Response {
    Response::from_bits_retain(Response::FAN_DENY.bits() | ((errno as u32) << 24))
}

/// Opens `path` in a thread so the main thread can answer the permission event.
fn open_in_thread(path: &Path) -> mpsc::Receiver<Result<Vec<u8>, String>> {
    let path = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let outcome = File::open(&path)
            .map_err(|e| format!("{e}"))
            .and_then(|mut f| {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map_err(|e| format!("{e}"))?;
                Ok(buf)
            });
        let _ = tx.send(outcome);
    });
    rx
}

/// Waits for one event, answers it with `answer`, and returns its mask.
fn answer_one(group: &Fanotify, answer: Response, timeout: Duration) -> Result<MaskFlags, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            return Err("no event within timeout".into());
        }
        match group.read_events() {
            Ok(events) if events.is_empty() => thread::sleep(Duration::from_millis(5)),
            Ok(events) => {
                let mut mask = MaskFlags::empty();
                for event in events {
                    mask = event.mask();
                    if let Some(fd) = event.fd() {
                        group
                            .write_response(FanotifyResponse::new(fd, answer))
                            .map_err(|e| format!("write_response: {e}"))?;
                    }
                }
                return Ok(mask);
            }
            Err(nix::errno::Errno::EAGAIN) => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("read_events: {e}")),
        }
    }
}

/// Reads whatever is queued right now, without waiting, and returns the mask of
/// each event found; an empty list means nothing arrived. Every event is
/// allowed, so an opener blocked on a permission event is released (a
/// notification event has no response, and the kernel rejects one — ignored).
fn drain_events(group: &Fanotify) -> Result<Vec<MaskFlags>, String> {
    match group.read_events() {
        Ok(events) => {
            let mut masks = Vec::new();
            for event in events {
                masks.push(event.mask());
                if let Some(fd) = event.fd() {
                    let _ = group.write_response(FanotifyResponse::new(fd, Response::FAN_ALLOW));
                }
            }
            Ok(masks)
        }
        Err(nix::errno::Errno::EAGAIN) => Ok(Vec::new()),
        Err(e) => Err(format!("read_events: {e}")),
    }
}

/// Runs `work` on another thread while this one answers (allows) every event it
/// raises, and reports the masks seen. Anything this process does to a file
/// inside a directory it has marked has to go through here: it cannot block in
/// `open()` and answer the event that blocks it at the same time.
fn while_answering<T: Send + 'static>(
    group: &Fanotify,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    timeout: Duration,
) -> Result<(T, Vec<MaskFlags>), String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(work());
    });
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        seen.extend(drain_events(group)?);
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(Ok(value)) => {
                // One last look: the event may still be on its way.
                thread::sleep(Duration::from_millis(20));
                seen.extend(drain_events(group)?);
                return Ok((value, seen));
            }
            Ok(Err(why)) => return Err(why),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() > deadline {
                    return Err(format!("worker stuck; events seen so far: {seen:?}"));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("worker died".into()),
        }
    }
}

/// A nameless file in `dir`, the first half of building a placeholder.
fn make_tmpfile(dir: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_TMPFILE)
        .mode(0o644)
        .open(dir)
        .map_err(|e| format!("O_TMPFILE in {dir:?}: {e}"))
}

/// Builds a file the way the design says a placeholder is built: nameless with
/// `O_TMPFILE` in the target directory, then linked into place. Returns nothing
/// but the error, if any.
fn tmpfile_then_link(dir: &Path, target: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;

    let mut file = make_tmpfile(dir)?;
    file.write_all(content).map_err(|e| format!("write: {e}"))?;
    let proc_path = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|e| format!("cstring: {e}"))?;
    let target_c = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|e| format!("cstring: {e}"))?;
    let rc = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            proc_path.as_ptr(),
            libc::AT_FDCWD,
            target_c.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if rc != 0 {
        return Err(format!(
            "linkat {target:?}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Fresh directory with one file of known content.
fn scratch(root: &Path, name: &str) -> Result<(PathBuf, PathBuf), String> {
    let dir = root.join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    let file = dir.join("payload.bin");
    fs::write(&file, b"hello").map_err(|e| format!("write: {e}"))?;
    Ok((dir, file))
}

/// A mark on the directory must produce a permission event for a file inside it.
fn dir_mark_intercepts(root: &Path) -> Result<(), String> {
    let (dir, file) = scratch(root, "poc-intercept")?;
    let group = group();
    mark_dir(&group, &dir)?;
    let opened = open_in_thread(&file);
    let mask = answer_one(&group, Response::FAN_ALLOW, Duration::from_secs(5))?;
    if !mask.contains(MaskFlags::FAN_OPEN_PERM) {
        return Err(format!("unexpected mask {mask:?}"));
    }
    match opened.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(content)) if content == b"hello" => Ok(()),
        other => Err(format!("reader got {other:?}")),
    }
}

/// THE GATE: an ignore mark on the file must suppress the event the parent
/// directory's mark would otherwise produce.
fn ignore_mark_suppresses(root: &Path) -> Result<(), String> {
    let (dir, file) = scratch(root, "poc-ignore")?;
    let group = group();
    mark_dir(&group, &dir)?;
    ignore_file(&group, &file)?;
    let opened = open_in_thread(&file);
    let read_back = opened.recv_timeout(Duration::from_secs(2));
    // Look for an event either way: if one arrived, the opener was blocked and
    // has to be released before we can report what happened.
    let queued = drain_events(&group)?;
    match read_back {
        Ok(Ok(content)) if content == b"hello" => {}
        other => {
            return Err(format!(
                "reader blocked or failed: {other:?} (events queued: {queued:?})"
            ))
        }
    }
    if !queued.is_empty() {
        return Err(format!(
            "{} event(s) arrived despite the ignore mark, masks {queued:?}",
            queued.len()
        ));
    }

    // The ignore mark is evictable, so the kernel may drop it whenever it
    // reclaims the inode. Force that and see what the next open does: events
    // coming back is the safe direction (the helper re-adds the mark), events
    // staying away would mean a file could be read unhydrated.
    nix::unistd::sync();
    fs::write("/proc/sys/vm/drop_caches", b"3\n").map_err(|e| format!("drop_caches: {e}"))?;
    let reopened = open_in_thread(&file);
    let after_eviction = answer_one(&group, Response::FAN_ALLOW, Duration::from_secs(2)).map_err(
        |why| format!("after drop_caches the open raised nothing ({why}) — eviction left the file unwatched"),
    )?;
    if !after_eviction.contains(MaskFlags::FAN_OPEN_PERM) {
        return Err(format!("after drop_caches the open raised {after_eviction:?}"));
    }
    match reopened.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(content)) if content == b"hello" => {}
        other => return Err(format!("reader after drop_caches got {other:?}")),
    }
    println!("    note: after drop_caches the same open raised {after_eviction:?} again");
    Ok(())
}

/// A file created after the directory was marked must still be covered. Along
/// the way this records what creating a file inside a marked directory costs
/// the creator: a plain create is itself intercepted, the design's
/// `O_TMPFILE` + `linkat` is not.
fn covers_new_files(root: &Path) -> Result<(), String> {
    let (dir, _) = scratch(root, "poc-newfile")?;
    let group = group();
    mark_dir(&group, &dir)?;

    let plain = dir.join("created-after.bin");
    let plain_path = plain.clone();
    let (_, during_create) = while_answering(
        &group,
        move || fs::write(&plain_path, b"later").map_err(|e| format!("write: {e}")),
        Duration::from_secs(5),
    )?;
    println!("    note: a plain create raised {} event(s) {during_create:?}", during_create.len());

    let tmp_dir = dir.clone();
    let (_, during_tmpfile) = while_answering(
        &group,
        move || {
            use std::io::Write;
            let mut f = make_tmpfile(&tmp_dir)?;
            f.write_all(b"nameless").map_err(|e| format!("write: {e}"))
        },
        Duration::from_secs(5),
    )?;
    println!(
        "    note: O_TMPFILE alone raised {} event(s) {during_tmpfile:?}",
        during_tmpfile.len()
    );

    let linked = dir.join("linked-in.bin");
    let (linked_dir, linked_target) = (dir.clone(), linked.clone());
    let (_, during_link) = while_answering(
        &group,
        move || tmpfile_then_link(&linked_dir, &linked_target, b"later"),
        Duration::from_secs(5),
    )?;
    println!(
        "    note: O_TMPFILE + linkat raised {} event(s) {during_link:?}",
        during_link.len()
    );

    let opened = open_in_thread(&linked);
    let mask = answer_one(&group, Response::FAN_ALLOW, Duration::from_secs(5))?;
    if !mask.contains(MaskFlags::FAN_OPEN_PERM) {
        return Err(format!("unexpected mask {mask:?}"));
    }
    match opened.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(content)) if content == b"later" => Ok(()),
        other => Err(format!("reader got {other:?}")),
    }
}

/// Without FAN_ONDIR, opening the directory itself must not be intercepted:
/// listing a folder must never wait for us.
fn dir_open_not_intercepted(root: &Path) -> Result<(), String> {
    let (dir, _) = scratch(root, "poc-diropen")?;
    let group = group();
    mark_dir(&group, &dir)?;
    let entries = fs::read_dir(&dir).map_err(|e| format!("read_dir: {e}"))?.count();
    if entries == 0 {
        return Err("read_dir returned nothing".into());
    }
    let queued = drain_events(&group)?;
    if queued.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} event(s) for a directory open, masks {queued:?}",
            queued.len()
        ))
    }
}

/// Denying with an errno must surface that errno to the opener.
fn deny_errno(root: &Path) -> Result<(), String> {
    let (dir, file) = scratch(root, "poc-deny")?;
    let group = group();
    mark_dir(&group, &dir)?;
    let opened = open_in_thread(&file);
    answer_one(&group, deny_with(libc::EIO), Duration::from_secs(5))?;
    match opened.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(message)) if message.contains("Input/output error") => Ok(()),
        Ok(Err(message)) if message.contains("Operation not permitted") => {
            Err("errno was flattened to EPERM — FAN_DENY_ERRNO unsupported here".into())
        }
        other => Err(format!("reader got {other:?}")),
    }
}

/// Writing through the event fd must not generate further events (FMODE_NONOTIFY),
/// which is what makes filling a placeholder in place possible.
fn write_through_event_fd(root: &Path) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::io::FromRawFd;

    let (dir, file) = scratch(root, "poc-eventfd")?;
    let group = group();
    // FAN_MODIFY as well as FAN_OPEN_PERM: without it nothing in this group
    // could ever report our own write, and the check would prove nothing.
    mark_dir_with(
        &group,
        &dir,
        MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_MODIFY | MaskFlags::FAN_EVENT_ON_CHILD,
    )?;
    let opened = open_in_thread(&file);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            return Err("no event".into());
        }
        let events = match group.read_events() {
            Ok(events) => events,
            Err(nix::errno::Errno::EAGAIN) => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(e) => return Err(format!("read_events: {e}")),
        };
        if events.is_empty() {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        for event in events {
            if !event.mask().contains(MaskFlags::FAN_OPEN_PERM) {
                return Err(format!("unexpected event before our write: {:?}", event.mask()));
            }
            let fd: BorrowedFd<'_> = event.fd().ok_or("queue overflow")?;
            // Write through a dup of the event fd, then answer.
            let dup = nix::unistd::dup(fd).map_err(|e| format!("dup: {e}"))?;
            let mut writer = unsafe { File::from_raw_fd(dup.as_raw_fd()) };
            std::mem::forget(dup);
            writer
                .seek(SeekFrom::Start(0))
                .map_err(|e| format!("seek: {e}"))?;
            writer.write_all(b"HELLO").map_err(|e| format!("write: {e}"))?;
            writer.sync_all().map_err(|e| format!("fsync: {e}"))?;
            drop(writer);
            group
                .write_response(FanotifyResponse::new(fd, Response::FAN_ALLOW))
                .map_err(|e| format!("write_response: {e}"))?;
        }
        break;
    }

    match opened.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(content)) if content == b"HELLO" => {}
        other => return Err(format!("reader got {other:?}")),
    }
    let queued = drain_events(&group)?;
    if !queued.is_empty() {
        return Err(format!(
            "{} recursive event(s) from our own write, masks {queued:?}",
            queued.len()
        ));
    }

    // Positive control. Silence above proves `FMODE_NONOTIFY` only if the same
    // write through an ordinary descriptor is loud: otherwise a mask that
    // reports nothing at all would look identical.
    let control_target = file.clone();
    let (_, control) = while_answering(
        &group,
        move || {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .open(&control_target)
                .map_err(|e| format!("open for control write: {e}"))?;
            f.write_all(b"AGAIN").map_err(|e| format!("control write: {e}"))?;
            f.sync_all().map_err(|e| format!("control fsync: {e}"))
        },
        Duration::from_secs(5),
    )?;
    let seen = control
        .iter()
        .fold(MaskFlags::empty(), |acc, mask| acc | *mask);
    if !seen.contains(MaskFlags::FAN_OPEN_PERM) || !seen.contains(MaskFlags::FAN_MODIFY) {
        return Err(format!(
            "positive control raised {control:?}; expected FAN_OPEN_PERM and FAN_MODIFY, \
             so the silence above proves nothing"
        ));
    }
    println!("    note: the same write through an ordinary fd raised {control:?}");
    Ok(())
}

/// Control for the documentation: a plain `FAN_DENY`, with no errno in the top
/// byte, reaches the opener as `EPERM`. This is what the helper would fall back
/// to if `FAN_DENY_ERRNO` were unavailable.
fn plain_deny_is_eperm(root: &Path) -> Result<(), String> {
    let (dir, file) = scratch(root, "poc-deny-plain")?;
    let group = group();
    mark_dir(&group, &dir)?;
    let opened = open_in_thread(&file);
    answer_one(&group, Response::FAN_DENY, Duration::from_secs(5))?;
    match opened.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(message)) if message.contains("Operation not permitted") => Ok(()),
        other => Err(format!("reader got {other:?}, expected EPERM")),
    }
}

/// Control for the documentation: `fanotify_mark` will not take an `O_PATH`
/// descriptor, which is why files are marked by (dirfd, name) instead. If a
/// later kernel starts accepting it, this check fails and the design gains an
/// option.
fn o_path_fd_rejected(root: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let (_dir, file) = scratch(root, "poc-opath")?;
    let group = group();
    let fd = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(&file)
        .map_err(|e| format!("open O_PATH {file:?}: {e}"))?;
    match group.mark(
        MarkFlags::FAN_MARK_ADD,
        MaskFlags::FAN_OPEN_PERM,
        fd.as_fd(),
        None::<&Path>,
    ) {
        Err(nix::errno::Errno::EBADF) => Ok(()),
        Err(other) => Err(format!("rejected with {other}, expected EBADF")),
        Ok(()) => Err("an O_PATH descriptor was accepted".into()),
    }
}

/// Creates `dirs` directories and marks them, reporting slab growth per mark.
fn measure(root: &Path, dirs: usize) -> Result<(), String> {
    let base = root.join("poc-measure");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).map_err(|e| format!("mkdir: {e}"))?;
    // Create everything first, so the measurement covers the marks and not the
    // inodes and dentries the creation itself allocates.
    let mut paths = Vec::with_capacity(dirs);
    for i in 0..dirs {
        let dir = base.join(format!("d{i}"));
        fs::create_dir(&dir).map_err(|e| format!("mkdir: {e}"))?;
        paths.push(dir);
    }
    measure_marks(
        "directory marks (FAN_OPEN_PERM|FAN_EVENT_ON_CHILD)",
        &paths,
        &|group, dir| mark_dir(group, dir),
    )
}

/// Marks every path, then reports what that cost while the marks are resident
/// and what is left once the kernel has reclaimed everything it may.
fn measure_marks(
    label: &str,
    paths: &[PathBuf],
    mark: &dyn Fn(&Fanotify, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let before = quiesce_and_snapshot()?;
    let group = group();
    for path in paths {
        mark(&group, path)?;
    }
    let resident = slab_snapshot()?;
    let reclaimed = quiesce_and_snapshot()?;
    report(&format!("{label} — resident"), paths.len(), &before, &resident);
    report(
        &format!("{label} — after drop_caches"),
        paths.len(),
        &before,
        &reclaimed,
    );
    drop(group);
    Ok(())
}

/// The same measurement for marks placed on files, one per file: this is what
/// the rejected "mark every file" design would have cost.
fn measure_files(root: &Path, files: usize) -> Result<(), String> {
    let base = root.join("poc-measure-files");
    let paths = populate(&base, files)?;
    measure_marks("file marks (FAN_OPEN_PERM)", &paths, &|group, file| {
        mark_file(
            group,
            file,
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM,
        )
    })
}

/// And for the evictable ignore marks this design puts on hydrated files.
fn measure_ignored_files(root: &Path, files: usize) -> Result<(), String> {
    let base = root.join("poc-measure-ignores");
    let paths = populate(&base, files)?;
    measure_marks("evictable ignore marks on files", &paths, &|group, file| {
        ignore_file(group, file)
    })
}

fn populate(base: &Path, files: usize) -> Result<Vec<PathBuf>, String> {
    let _ = fs::remove_dir_all(base);
    fs::create_dir_all(base).map_err(|e| format!("mkdir: {e}"))?;
    let mut paths = Vec::with_capacity(files);
    for i in 0..files {
        let file = base.join(format!("f{i}"));
        fs::write(&file, b"x").map_err(|e| format!("write: {e}"))?;
        paths.push(file);
    }
    Ok(paths)
}

/// The caches a mark itself lives in; everything else a mark costs is the inode
/// and dentry it pins, which show up in the per-cache breakdown.
const MARK_CACHES: [&str; 3] = [
    "fanotify_mark",
    "fsnotify_inode_mark_connector",
    "fsnotify_mark_connector",
];

fn report(label: &str, count: usize, before: &SlabSnapshot, after: &SlabSnapshot) {
    let total = after.total as i64 - before.total as i64;
    println!(
        "{label}: {count} marks, slab grew {total} bytes, {:.1} bytes per mark",
        total as f64 / count as f64
    );

    let mut deltas: Vec<(&str, i64)> = Vec::new();
    for (name, after_bytes) in &after.caches {
        let before_bytes = before.caches.get(name).copied().unwrap_or(0) as i64;
        let delta = *after_bytes as i64 - before_bytes;
        if delta != 0 {
            deltas.push((name.as_str(), delta));
        }
    }
    deltas.sort_by_key(|(_, delta)| -delta.abs());

    // The total is the sum over every cache, so listing the movers accounts for
    // all of it; the tail is printed as one line rather than dropped.
    let structures: i64 = deltas
        .iter()
        .filter(|(name, _)| MARK_CACHES.contains(name))
        .map(|(_, delta)| *delta)
        .sum();
    const SHOWN: usize = 8;
    for (name, delta) in deltas.iter().take(SHOWN) {
        println!(
            "    {name}: {delta:+} bytes ({:.1} per mark)",
            *delta as f64 / count as f64
        );
    }
    if deltas.len() > SHOWN {
        let tail: i64 = deltas.iter().skip(SHOWN).map(|(_, delta)| *delta).sum();
        println!(
            "    {} further caches: {tail:+} bytes ({:.1} per mark)",
            deltas.len() - SHOWN,
            tail as f64 / count as f64
        );
    }
    println!(
        "    fanotify structures alone: {structures:+} bytes ({:.1} per mark)",
        structures as f64 / count as f64
    );
}

struct SlabSnapshot {
    total: u64,
    caches: std::collections::BTreeMap<String, u64>,
}

/// Frees everything reclaimable, so the snapshot reflects what is pinned rather
/// than what happens to be cached, and takes a snapshot. A mark pins the inode
/// it is attached to, so an honest per-mark figure has to include that.
fn quiesce_and_snapshot() -> Result<SlabSnapshot, String> {
    nix::unistd::sync();
    fs::write("/proc/sys/vm/drop_caches", b"3\n")
        .map_err(|e| format!("drop_caches: {e}"))?;
    thread::sleep(Duration::from_millis(200));
    slab_snapshot()
}

/// Every slab cache's `active_objs × objsize`, and their sum. That counts the
/// bytes in objects currently handed out: it deliberately excludes free objects
/// sitting in already-allocated slabs and the per-slab slack, so it tracks what
/// the marks hold rather than how much memory the allocator is sitting on. SLUB
/// reports `active_objs` approximately — objects parked in per-cpu partial slabs
/// are not counted — so individual figures run a few percent low.
fn slab_snapshot() -> Result<SlabSnapshot, String> {
    let text = fs::read_to_string("/proc/slabinfo").map_err(|e| format!("slabinfo: {e}"))?;
    let mut total = 0u64;
    let mut caches = std::collections::BTreeMap::new();
    for line in text.lines().skip(2) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() > 5 {
            let objects: u64 = cols[1].parse().unwrap_or(0);
            let size: u64 = cols[3].parse().unwrap_or(0);
            total += objects * size;
            caches.insert(cols[0].to_string(), objects * size);
        }
    }
    Ok(SlabSnapshot { total, caches })
}
