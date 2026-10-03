use std::ffi::CString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};

use crate::{USER, fan};
use crate::sys::{
    ename, fan_init, io_err, last_errno, name_to_handle, ok_or, open_by_handle, read_sysctl,
    statfs_of,
};

// ---------------------------------------------------------------------------
// Root side
// ---------------------------------------------------------------------------

const FILESYSTEMS: [(&str, i64); 3] =
    [("btrfs", 0x9123_683E), ("ext4", 0x0000_EF53), ("xfs", 0x5846_5342)];

pub(crate) fn root_main(fs_name: &str) -> i32 {
    let mut failures = 0;
    let mnt = PathBuf::from(format!("/mnt/{fs_name}"));
    let Some((_, magic)) = FILESYSTEMS.iter().find(|(n, _)| *n == fs_name) else {
        println!("FAIL unknown --fs {fs_name}");
        return 1;
    };
    if unsafe { libc::getuid() } != 0 {
        println!("FAIL the probe must start as root, inside the VM");
        return 1;
    }
    let got = statfs_of(&mnt).f_type as i64;
    if got != *magic {
        println!(
            "FAIL {mnt:?} reports f_type {got:#x}, expected {magic:#x} — the mount did not happen"
        );
        return 1;
    }
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    println!("watch-probe: kernel {}, filesystem {fs_name} at {mnt:?}", release.trim());

    println!("== /proc/sys/fs/fanotify (read as root)");
    let mut names: Vec<String> = fs::read_dir("/proc/sys/fs/fanotify")
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    for n in &names {
        println!("  {n} = {}", read_sysctl(n));
    }

    let w = mnt.join("wprobe");
    let _ = fs::remove_dir_all(&w);
    mkdir_owned(&w);

    root_facts(&w);

    // --- what an unprivileged process may do at all
    let flags_dir = w.join("flags");
    mkdir_owned(&flags_dir);
    let rootonly = w.join("rootonly");
    fs::create_dir(&rootonly).unwrap();
    fs::set_permissions(&rootonly, fs::Permissions::from_mode(0o700)).unwrap();
    let tmpfs_dir = PathBuf::from("/mnt/wprobe-tmpfs");
    let _ = fs::remove_dir_all(&tmpfs_dir);
    mkdir_owned(&tmpfs_dir);
    let subvol = if fs_name == "btrfs" {
        let sv = w.join("subvol");
        let out = Command::new("btrfs").arg("subvolume").arg("create").arg(&sv).output();
        match out {
            Ok(o) if o.status.success() => {
                chown_user(&sv);
                sv.to_string_lossy().into_owned()
            }
            other => {
                println!("  (btrfs subvolume create failed: {other:?})");
                "-".into()
            }
        }
    } else {
        "-".into()
    };
    let fsroot = mnt.to_string_lossy().into_owned();
    failures += run_child_inherit(
        "flags",
        &[
            &flags_dir.to_string_lossy(),
            &rootonly.to_string_lossy(),
            &fsroot,
            &tmpfs_dir.to_string_lossy(),
            &subvol,
        ],
    );

    // --- the event matrix, alone
    let m1 = w.join("m1");
    prepare_matrix(&m1);
    println!("== event matrix: the design's group alone (uid {USER})");
    let (ok1, solo) = run_child_captured("matrix", &[&m1.to_string_lossy()]);
    print!("{solo}");
    if !ok1 {
        println!("FAIL the matrix child did not finish cleanly");
        failures += 1;
    }

    // --- the same matrix while a root pre-content group marks the same directories
    let helper = HelperSim::start();
    let m2 = w.join("m2");
    prepare_matrix(&m2);
    for d in [m2.join("tree"), m2.join("tree/A"), m2.join("tree/B")] {
        helper.mark(&d);
    }
    println!(
        "== event matrix again, while a root FAN_CLASS_PRE_CONTENT group holds FAN_OPEN_PERM|FAN_EVENT_ON_CHILD \
         marks on tree, A and B and allows every open"
    );
    let (ok2, coexist) = run_child_captured("matrix", &[&m2.to_string_lossy()]);
    if !ok2 {
        println!("FAIL the second matrix child did not finish cleanly");
        failures += 1;
    }
    compare_runs(&solo, &coexist);
    println!("  pre-content events the root group answered: {}", helper.answered());

    // --- fills and denials through the root group's event descriptors
    let fill = w.join("fill");
    mkdir_owned(&fill);
    for name in FILL_FILES {
        let p = fill.join(name);
        fs::write(&p, vec![b'a'; 8192]).unwrap();
        chown_user(&p);
    }
    helper.mark(&fill);
    println!(
        "== writes through a pre-content event descriptor (the helper's fill path), seen by the unprivileged group"
    );
    failures += run_child_inherit("fill", &[&fill.to_string_lossy()]);
    for note in helper.notes() {
        println!("  root group: {note}");
    }
    drop(helper);

    // --- queue overflow
    let ovf = w.join("overflow");
    mkdir_owned(&ovf);
    println!("== queue overflow (uid {USER})");
    failures += run_child_inherit("overflow", &[&ovf.to_string_lossy()]);

    // --- per-user mark limit, lowered for the test and put back
    let ml = w.join("marklimit");
    mkdir_owned(&ml);
    let saved = read_sysctl("max_user_marks");
    let lowered = "40";
    match fs::write("/proc/sys/fs/fanotify/max_user_marks", lowered) {
        Ok(()) => {
            println!("== per-user mark limit: max_user_marks lowered from {saved} to {lowered} for this step");
            failures += run_child_inherit("marklimit", &[&ml.to_string_lossy()]);
            let _ = fs::write("/proc/sys/fs/fanotify/max_user_marks", &saved);
            println!("  max_user_marks restored to {}", read_sysctl("max_user_marks"));
        }
        Err(e) => println!("== per-user mark limit: cannot lower max_user_marks: {}", io_err(&e)),
    }

    println!("failures={failures}");
    if failures == 0 {
        0
    } else {
        1
    }
}

fn chown_user(p: &Path) {
    std::os::unix::fs::lchown(p, Some(USER), Some(USER))
        .unwrap_or_else(|e| panic!("chown {p:?}: {e}"));
}

fn mkdir_owned(p: &Path) {
    fs::create_dir_all(p).unwrap_or_else(|e| panic!("mkdir {p:?}: {e}"));
    chown_user(p);
}

fn prepare_matrix(base: &Path) {
    let _ = fs::remove_dir_all(base);
    for d in ["", "tree", "tree/A", "tree/B", "out"] {
        mkdir_owned(&base.join(d));
    }
}

/// What root can do that the daemon cannot, and the design's premise that FID
/// reporting is refused to the permission classes.
fn root_facts(w: &Path) {
    println!("== fanotify_init as root (the helper's side)");
    let cases: [(&str, u32); 6] = [
        ("FAN_CLASS_PRE_CONTENT | FAN_REPORT_FID", fan::CLASS_PRE_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT | FAN_REPORT_DFID_NAME", fan::CLASS_PRE_CONTENT | fan::REPORT_DFID_NAME),
        ("FAN_CLASS_CONTENT | FAN_REPORT_FID", fan::CLASS_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT (the helper's, no FID)", fan::CLASS_PRE_CONTENT | fan::UNLIMITED_QUEUE | fan::UNLIMITED_MARKS),
        ("the design's notification group + FAN_UNLIMITED_QUEUE | FAN_UNLIMITED_MARKS", fan::DESIGN_INIT | fan::UNLIMITED_QUEUE | fan::UNLIMITED_MARKS),
        ("the design's notification group + FAN_REPORT_PIDFD", fan::DESIGN_INIT | fan::REPORT_PIDFD),
    ];
    for (label, flags) in cases {
        let ev = (libc::O_RDONLY | libc::O_CLOEXEC) as u32;
        println!("  {label}: {}", ok_or(fan_init(flags, ev)));
    }
    // Control for the unprivileged refusal below.
    let dir = File::open(w).unwrap();
    match name_to_handle(w, 0) {
        Ok((h, _)) => {
            let r = open_by_handle(dir.as_raw_fd(), &h, libc::O_RDONLY | libc::O_DIRECTORY);
            println!("  open_by_handle_at as root (control): {}", ok_or(r));
        }
        Err(e) => println!("  name_to_handle_at as root: {}", ename(e)),
    }
}

fn child_command(mode: &str, args: &[&str]) -> Command {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.arg("--child").arg(mode).args(args);
    // std calls setgroups(0) before setuid when the parent is root, so the
    // child keeps no supplementary group; the kernel clears every capability
    // on the switch from uid 0 to a non-zero uid.
    cmd.gid(USER).uid(USER);
    cmd
}

fn wait_bounded(child: &mut std::process::Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                println!("  (child killed after {limit:?})");
                return false;
            }
        }
    }
}

fn run_child_inherit(mode: &str, args: &[&str]) -> u32 {
    let mut child = child_command(mode, args).spawn().expect("spawn child");
    if wait_bounded(&mut child, Duration::from_secs(200)) {
        0
    } else {
        println!("FAIL child {mode} did not finish cleanly");
        1
    }
}

fn run_child_captured(mode: &str, args: &[&str]) -> (bool, String) {
    let mut child = child_command(mode, args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let mut stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let ok = wait_bounded(&mut child, Duration::from_secs(200));
    (ok, reader.join().unwrap_or_default())
}

/// An `O_TMPFILE`'s events carry the pseudo-name `#<inode number>`, which
/// differs between two runs for no reason that matters here.
fn normalize_tmpfile_names(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("\"#") {
        out.push_str(&rest[..i + 2]);
        let tail = &rest[i + 2..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && tail[digits..].starts_with('"') {
            out.push_str("ino");
            rest = &tail[digits..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn compare_runs(a: &str, b: &str) {
    let la: Vec<String> = a.lines().map(normalize_tmpfile_names).collect();
    let lb: Vec<String> = b.lines().map(normalize_tmpfile_names).collect();
    if la == lb {
        println!(
            "  identical to the run without the pre-content group: {} lines, every event the same \
             (an O_TMPFILE's \"#<inode>\" pseudo-name aside)",
            la.len()
        );
        return;
    }
    println!("  DIFFERS from the run without the pre-content group:");
    for i in 0..la.len().max(lb.len()) {
        let x = la.get(i).map(String::as_str).unwrap_or("<none>");
        let y = lb.get(i).map(String::as_str).unwrap_or("<none>");
        if x != y {
            println!("    line {i}: alone:   {x}");
            println!("    line {i}: coexist: {y}");
        }
    }
}

pub(crate) const FILL_FILES: [&str; 7] = [
    "fill-control",
    "fill-write",
    "fill-trunc",
    "fill-times",
    "fill-xattr",
    "fill-punch",
    "fill-own",
];

/// A minimal stand-in for `konedrive-helper`: a pre-content group set up as
/// the helper sets its own up, marking directories with `FAN_OPEN_PERM |
/// FAN_EVENT_ON_CHILD`, answering from its own thread. For a file named
/// `fill-*` it first does one kind of write through the event descriptor,
/// the way a fill writes; `denyme*` is denied.
struct HelperSim {
    group: Arc<Fanotify>,
    stop: Arc<AtomicBool>,
    notes: Arc<Mutex<Vec<String>>>,
    count: Arc<Mutex<u64>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HelperSim {
    fn start() -> Self {
        let group = Arc::new(
            Fanotify::init(
                InitFlags::FAN_CLASS_PRE_CONTENT
                    | InitFlags::FAN_CLOEXEC
                    | InitFlags::FAN_UNLIMITED_QUEUE
                    | InitFlags::FAN_UNLIMITED_MARKS
                    | InitFlags::FAN_NONBLOCK,
                EventFFlags::O_RDWR
                    | EventFFlags::O_LARGEFILE
                    | EventFFlags::O_CLOEXEC
                    | EventFFlags::O_NONBLOCK,
            )
            .expect("pre-content fanotify_init as root"),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(Mutex::new(0u64));
        let (g, s, n, c) = (group.clone(), stop.clone(), notes.clone(), count.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match g.read_events() {
                    Ok(events) if events.is_empty() => thread::sleep(Duration::from_millis(2)),
                    Ok(events) => {
                        for event in events {
                            let Some(fd) = event.fd() else {
                                n.lock().unwrap().push("overflow".into());
                                continue;
                            };
                            let path = fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
                                .unwrap_or_default();
                            let name = path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let mut answer = Response::FAN_ALLOW;
                            if let Some(note) = fill_through(fd.as_raw_fd(), &name) {
                                n.lock().unwrap().push(format!("{name}: {note}"));
                            }
                            if name.starts_with("denyme") {
                                answer = Response::FAN_DENY;
                                n.lock().unwrap().push(format!("{name}: FAN_DENY"));
                            }
                            *c.lock().unwrap() += 1;
                            if let Err(e) = g.write_response(FanotifyResponse::new(fd, answer)) {
                                n.lock().unwrap().push(format!("{name}: write_response {e}"));
                            }
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => thread::sleep(Duration::from_millis(2)),
                    Err(e) => {
                        n.lock().unwrap().push(format!("read_events: {e}"));
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        });
        HelperSim { group, stop, notes, count, thread: Some(thread) }
    }

    fn mark(&self, dir: &Path) {
        let fd = File::open(dir).unwrap();
        self.group
            .mark(
                MarkFlags::FAN_MARK_ADD,
                MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
                fd.as_fd(),
                None::<&Path>,
            )
            .unwrap_or_else(|e| panic!("pre-content mark {dir:?}: {e}"));
    }

    fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }

    fn answered(&self) -> u64 {
        *self.count.lock().unwrap()
    }
}

impl Drop for HelperSim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One kind of write through a pre-content event descriptor, chosen by name.
fn fill_through(fd: RawFd, name: &str) -> Option<String> {
    let rc = |r: libc::c_int| if r < 0 { ename(last_errno()) } else { "ok".into() };
    Some(match name {
        "fill-write" => {
            let n = unsafe { libc::pwrite(fd, b"HELLO".as_ptr().cast(), 5, 0) };
            format!("pwrite 5 bytes through the event descriptor: {}", rc(n as i32))
        }
        "fill-trunc" => format!(
            "ftruncate(16384) through the event descriptor: {}",
            rc(unsafe { libc::ftruncate(fd, 16384) })
        ),
        "fill-times" => {
            let ts = [
                libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
                libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
            ];
            format!(
                "futimens through the event descriptor: {}",
                rc(unsafe { libc::futimens(fd, ts.as_ptr()) })
            )
        }
        "fill-xattr" => {
            let key = CString::new("user.konedrive.state").unwrap();
            let r = unsafe {
                libc::fsetxattr(fd, key.as_ptr(), b"hydrated".as_ptr().cast(), 8, 0)
            };
            format!("fsetxattr user.konedrive.state through the event descriptor: {}", rc(r))
        }
        "fill-punch" => format!(
            "fallocate(PUNCH_HOLE|KEEP_SIZE, 0, 4096) through the event descriptor: {}",
            rc(unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    0,
                    4096,
                )
            })
        ),
        _ => return None,
    })
}
