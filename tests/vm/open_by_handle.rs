//! `OpenByHandle` (`docs/design/writes.md` §8.2; SECURITY.md): the helper's one new
//! message, which hands the daemon a descriptor for an object of its own that
//! left the folder.
//!
//! Two parts.
//!
//! **The measurement** (`docs/kernel-behavior-7.2.md` §15), run by
//! `tests/vm/run.sh unit`: `--obh-serve` runs as a service with **the helper
//! unit's own sandbox** (`helper_unit_test.sh` copies the shipped unit and
//! swaps its `ExecStart`), receives a directory descriptor from a process
//! outside that sandbox, as the helper does from a daemon, and reports what
//! `open_by_handle_at` relative to it gives for each kind of object and each
//! set of flags; and whether its own opens raise `FAN_OPEN_PERM` in a group it
//! holds. `--obh-measure` is the other side. Neither decides anything: the
//! report is printed as it is.
//!
//! **The checks**: the shipped helper's `OpenByHandle` in the suite
//! (`tests/vm/run.sh quick --only OpenByHandle`) — a moved-out placeholder and
//! directory, every refusal, and the helper's own open under its own marks —
//! and under the shipped unit (`unit_check`, from unit.rs).

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{create_placeholder, State, XATTR_ITEM_ID};
use konedrive_proto::Channel;
use konedrived::sync::helper::{reopen_for_writing, HelperError, HelperLink};
use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};
use nix::sys::socket::{
    accept, bind, connect, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};

use crate::{dir_mark_present, ignore_mark_present, Checks, Ctx};

/// The uid the measured objects belong to.
const USER: u32 = 1000;

// ---------------------------------------------------------------------------
// the measurement: the client, outside the sandbox
// ---------------------------------------------------------------------------

/// `vm-scenarios --obh-measure <base> <socket>`, as root, outside the
/// sandbox: lays out `<base>/d` with objects of uid 1000, sends `d`'s
/// descriptor and their handles to the probe service, and prints its report.
pub(crate) fn probe_measure(base: &Path, socket_path: &Path) -> i32 {
    match measure_client(base, socket_path) {
        Ok(report) => {
            println!("{report}");
            0
        }
        Err(why) => {
            println!("FAIL the measurement did not run: {why}");
            1
        }
    }
}

fn measure_client(base: &Path, socket_path: &Path) -> Result<String, String> {
    let d = base.join("d");
    let _ = std::fs::remove_dir_all(base);
    std::fs::create_dir_all(&d).map_err(|e| format!("cannot create {d:?}: {e}"))?;
    let chown = |path: &Path| {
        std::os::unix::fs::chown(path, Some(USER), Some(USER)).map_err(|e| format!("chown {path:?}: {e}"))
    };
    let chmod = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {path:?}: {e}"))
    };
    chown(&d)?;
    for (name, mode) in [("f644", 0o644), ("f666", 0o666), ("f600", 0o600)] {
        let path = d.join(name);
        std::fs::write(&path, b"probe").map_err(|e| format!("cannot write {path:?}: {e}"))?;
        chown(&path)?;
        chmod(&path, mode)?;
        xattr::set(&path, "user.konedrive.item-id", b"PROBE").map_err(|e| format!("setxattr {path:?}: {e}"))?;
    }
    let sub = d.join("sub");
    std::fs::create_dir(&sub).map_err(|e| format!("mkdir {sub:?}: {e}"))?;
    chown(&sub)?;
    xattr::set(&sub, "user.konedrive.item-id", b"PROBEDIR").map_err(|e| format!("setxattr {sub:?}: {e}"))?;

    // A Btrfs subvolume inside `d`: a different `st_dev`, the same filesystem.
    let vol = d.join("vol");
    let made = Command::new("btrfs")
        .args(["-q", "subvolume", "create"])
        .arg(&vol)
        .status()
        .map_err(|e| format!("cannot run btrfs: {e}"))?;
    if !made.success() {
        return Err(format!("btrfs subvolume create {vol:?} failed: {made}"));
    }
    let in_vol = vol.join("f");
    std::fs::write(&in_vol, b"probe").map_err(|e| e.to_string())?;
    chown(&in_vol)?;
    xattr::set(&in_vol, "user.konedrive.item-id", b"PROBEVOL").map_err(|e| e.to_string())?;

    // A file on another filesystem altogether (the guest's /run, tmpfs).
    let tmp = Path::new("/run/obh-probe-tmpfs");
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let on_tmpfs = tmp.join("f");
    std::fs::write(&on_tmpfs, b"probe").map_err(|e| e.to_string())?;
    chown(&on_tmpfs)?;

    let dir = File::open(&d).map_err(|e| e.to_string())?;
    let handle_of = |parent: &Path, name: &str| -> Result<FileHandle, String> {
        let parent = File::open(parent).map_err(|e| e.to_string())?;
        FileHandle::at(&parent, std::ffi::OsStr::new(name)).map_err(|e| format!("name_to_handle_at {name}: {e}"))
    };
    let mut request = format!("path {}\n", d.display());
    for (label, parent, name) in [
        ("f644", d.as_path(), "f644"),
        ("f666", d.as_path(), "f666"),
        ("f600", d.as_path(), "f600"),
        ("sub", d.as_path(), "sub"),
        ("vol/f", vol.as_path(), "f"),
        ("tmpfs/f", tmp, "f"),
    ] {
        let handle = handle_of(parent, name)?;
        request.push_str(&format!("h {label} {} {}\n", handle.kind, hex(&handle.bytes)));
    }

    // The service binds a moment after systemd starts it.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut channel = loop {
        match connect_to(socket_path) {
            Ok(channel) => break channel,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("cannot connect to {socket_path:?}: {e}")),
        }
    };
    channel.send(&request, Some(dir.as_fd())).map_err(|e| e.to_string())?;
    let (report, _) = channel.recv::<String>().map_err(|e| format!("no report: {e}"))?;
    Ok(report)
}

fn connect_to(path: &Path) -> std::io::Result<Channel> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
    connect(fd.as_raw_fd(), &UnixAddr::new(path)?)?;
    Channel::new(UnixStream::from(fd))
}

// ---------------------------------------------------------------------------
// the measurement: the service, inside the helper's sandbox
// ---------------------------------------------------------------------------

/// `vm-scenarios --obh-serve <socket>`, started by systemd from a copy of the
/// helper's unit: answers one client with the report, then exits.
pub(crate) fn probe_serve(socket_path: &Path) -> i32 {
    let run = || -> Result<(), String> {
        let _ = std::fs::remove_file(socket_path);
        let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .map_err(|e| e.to_string())?;
        bind(listener.as_raw_fd(), &UnixAddr::new(socket_path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("bind: {e}"))?;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| e.to_string())?;
        listen(&listener, Backlog::new(1).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let conn = accept(listener.as_raw_fd()).map_err(|e| format!("accept: {e}"))?;
        // SAFETY: `accept` just returned this descriptor, owned by no one else.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(conn) }).map_err(|e| e.to_string())?;
        let (request, anchor) = channel.recv::<String>().map_err(|e| e.to_string())?;
        let anchor = anchor.ok_or("the request came without a directory descriptor")?;
        let report = serve_report(&request, anchor);
        channel.send(&report, None).map_err(|e| e.to_string())
    };
    match run() {
        Ok(()) => 0,
        Err(why) => {
            println!("obh-serve: {why}");
            1
        }
    }
}

const TRIED: [(&str, libc::c_int); 4] = [
    ("O_PATH", libc::O_PATH),
    ("O_RDONLY", libc::O_RDONLY),
    ("O_RDWR", libc::O_RDWR),
    ("O_RDONLY|O_DIRECTORY", libc::O_RDONLY | libc::O_DIRECTORY),
];

/// Every flag set is tried with these too: the helper's own choice for a
/// regular file (design §4.6).
const ALWAYS: libc::c_int = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

fn serve_report(request: &str, anchor: OwnedFd) -> String {
    let mut out: Vec<String> = Vec::new();
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_owned()))
            .unwrap_or_default()
    };
    out.push(format!(
        "the service: uid {}, CapEff {}, NoNewPrivs {}, Seccomp {}",
        nix::unistd::geteuid(),
        field("CapEff:"),
        field("NoNewPrivs:"),
        field("Seccomp:")
    ));

    let mut own_path = None;
    let mut handles: Vec<(String, FileHandle)> = Vec::new();
    for line in request.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["path", path] => own_path = Some(path.to_string()),
            ["h", label, kind, bytes] => {
                if let (Ok(kind), Some(bytes)) = (kind.parse(), unhex(bytes)) {
                    handles.push((label.to_string(), FileHandle { kind, bytes }));
                }
            }
            _ => {}
        }
    }
    let anchor_meta = fstat(anchor.as_fd());
    out.push(format!(
        "the daemon's directory descriptor: {}; its mount {}",
        anchor_meta.as_ref().map(describe).unwrap_or_else(|e| e.clone()),
        mount_mode(anchor.as_fd())
    ));
    let own = own_path.as_deref().map(|path| {
        File::open(path).map_err(|e| format!("cannot open {path} in the service's own namespace: {e}"))
    });
    match &own {
        Some(Ok(dir)) => out.push(format!(
            "the same directory opened by the service itself: its mount {}",
            mount_mode(dir.as_fd())
        )),
        Some(Err(why)) => out.push(why.clone()),
        None => {}
    }

    out.push(String::new());
    out.push(format!("open_by_handle_at(the daemon's descriptor, handle, flags | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK):"));
    for (label, handle) in &handles {
        for (name, flags) in TRIED {
            let result = handle.open(anchor.as_fd(), flags | ALWAYS);
            let mut line = format!("  {label:<8} {name:<22} ");
            match result {
                Ok(fd) => {
                    let meta = fstat(fd.as_fd());
                    line.push_str(&format!(
                        "ok: {}",
                        meta.as_ref()
                            .map(|m| {
                                let same = anchor_meta.as_ref().is_ok_and(|a| a.st_dev == m.st_dev);
                                format!("{}, st_dev {}", describe(m), if same { "= the directory's" } else { "≠ the directory's" })
                            })
                            .unwrap_or_else(|e| e.clone())
                    ));
                    line.push_str(&format!("; item-id xattr {}", item_id_through(fd.as_fd())));
                    if flags == libc::O_RDWR {
                        // SAFETY: a one-byte write from a live buffer to a live descriptor.
                        let wrote = unsafe { libc::pwrite(fd.as_raw_fd(), b"W".as_ptr().cast(), 1, 0) };
                        line.push_str(if wrote == 1 { "; pwrite ok" } else { "; pwrite failed" });
                        if wrote != 1 {
                            line.push_str(&format!(" ({})", std::io::Error::last_os_error()));
                        }
                    }
                }
                Err(e) => line.push_str(&errno_name(&e)),
            }
            out.push(line);
        }
    }
    if let Some(Ok(own_dir)) = &own {
        out.push(String::new());
        out.push("relative to the directory the service opened itself (its read-only view):".into());
        for (label, handle) in handles.iter().filter(|(l, _)| l == "f666" || l == "f644") {
            for (name, flags) in [("O_RDONLY", libc::O_RDONLY), ("O_RDWR", libc::O_RDWR)] {
                let result = handle.open(own_dir.as_fd(), flags | ALWAYS);
                out.push(format!(
                    "  {label:<8} {name:<22} {}",
                    match result {
                        Ok(_) => "ok".to_owned(),
                        Err(e) => errno_name(&e),
                    }
                ));
            }
        }
    }

    out.push(String::new());
    out.push(self_events(&anchor, &handles));
    out.join("\n")
}

/// Whether the service's own opens by handle raise `FAN_OPEN_PERM` in a group
/// it holds itself, marked as the helper marks: the directory with
/// `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`, then one file on its own.
fn self_events(anchor: &OwnedFd, handles: &[(String, FileHandle)]) -> String {
    let mut out = vec![format!(
        "the service's own opens by handle, in a FAN_CLASS_PRE_CONTENT group of its own (pid {}):",
        std::process::id()
    )];
    let group = match Fanotify::init(
        InitFlags::FAN_CLASS_PRE_CONTENT | InitFlags::FAN_CLOEXEC | InitFlags::FAN_NONBLOCK,
        EventFFlags::O_RDONLY | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC | EventFFlags::O_NONBLOCK,
    ) {
        Ok(group) => group,
        Err(e) => return format!("{}\n  fanotify_init: {e}", out.join("\n")),
    };
    let find = |label: &str| handles.iter().find(|(l, _)| l == label).map(|(_, h)| h.clone());
    let dir_mask = MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD;
    if let Err(e) = group.mark(MarkFlags::FAN_MARK_ADD, dir_mask, anchor.as_fd(), None::<&Path>) {
        return format!("{}\n  cannot mark the directory: {e}", out.join("\n"));
    }
    let cases: [(&str, &str, libc::c_int); 4] = [
        ("directory marked", "f644", libc::O_PATH),
        ("directory marked", "f644", libc::O_RDONLY),
        ("directory marked", "f666", libc::O_RDWR),
        ("directory marked", "sub", libc::O_RDONLY | libc::O_DIRECTORY),
    ];
    for (setting, label, flags) in cases {
        if let Some(handle) = find(label) {
            out.push(one_self_open(&group, anchor, setting, label, &handle, flags));
        }
    }
    // The mark `MarkFile` places: `FAN_OPEN_PERM` on the file itself, the
    // directory's mark gone. Placed by name, as the helper never opens a file
    // to mark it.
    let _ = group.mark(MarkFlags::FAN_MARK_REMOVE, dir_mask, anchor.as_fd(), None::<&Path>);
    match group.mark(MarkFlags::FAN_MARK_ADD, MaskFlags::FAN_OPEN_PERM, anchor.as_fd(), Some(Path::new("f600"))) {
        Ok(()) => {
            if let Some(handle) = find("f600") {
                out.push(one_self_open(&group, anchor, "file marked", "f600", &handle, libc::O_RDONLY));
            }
        }
        Err(e) => out.push(format!("  cannot mark f600: {e}")),
    }
    out.join("\n")
}

fn one_self_open(
    group: &Fanotify,
    anchor: &OwnedFd,
    setting: &str,
    label: &str,
    handle: &FileHandle,
    flags: libc::c_int,
) -> String {
    let (tx, rx) = mpsc::channel();
    let anchor = anchor.try_clone().expect("dup");
    let handle = handle.clone();
    let started = Instant::now();
    std::thread::spawn(move || {
        let result = handle.open(anchor.as_fd(), flags | ALWAYS).map(drop);
        let _ = tx.send((result, started.elapsed()));
    });
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut opened = None;
    while Instant::now() < deadline && opened.is_none() {
        if let Ok(read) = group.read_events() {
            for event in read {
                events.push(if event.pid() == std::process::id() as i32 { "own pid" } else { "another pid" });
                if let Some(fd) = event.fd() {
                    let _ = group.write_response(FanotifyResponse::new(fd, Response::FAN_ALLOW));
                }
            }
        }
        opened = rx.try_recv().ok();
        std::thread::sleep(Duration::from_millis(5));
    }
    let name = TRIED.iter().find(|(_, f)| *f == flags).map(|(n, _)| *n).unwrap_or("?");
    let result = match opened {
        Some((Ok(()), took)) => format!("the open returned ok after {} ms", took.as_millis()),
        Some((Err(e), _)) => format!("the open failed {}", errno_name(&e)),
        None => "the open had not returned after 1.5 s".to_owned(),
    };
    format!(
        "  {setting:<17} {label:<5} {name:<22} {} FAN_OPEN_PERM event(s){}; {result}",
        events.len(),
        if events.is_empty() { String::new() } else { format!(" ({})", events.join(", ")) }
    )
}

fn fstat(fd: BorrowedFd<'_>) -> Result<libc::stat, String> {
    // SAFETY: `st` is a live, correctly sized `stat` that `fstat` fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(format!("fstat: {}", std::io::Error::last_os_error()));
    }
    Ok(st)
}

fn describe(st: &libc::stat) -> String {
    let kind = match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => "file",
        libc::S_IFDIR => "directory",
        _ => "other",
    };
    format!("{kind} {:o} uid {} nlink {}", st.st_mode & 0o7777, st.st_uid, st.st_nlink)
}

fn mount_mode(fd: BorrowedFd<'_>) -> &'static str {
    // SAFETY: a live, correctly sized `statvfs` that `fstatvfs` fills.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(fd.as_raw_fd(), &mut st) } != 0 {
        return "(fstatvfs failed)";
    }
    if st.f_flag & libc::ST_RDONLY != 0 {
        "is read-only"
    } else {
        "is read-write"
    }
}

fn item_id_through(fd: BorrowedFd<'_>) -> String {
    let mut buf = [0u8; 64];
    // SAFETY: a live buffer of the size given, a NUL-terminated name.
    let n = unsafe {
        libc::fgetxattr(fd.as_raw_fd(), c"user.konedrive.item-id".as_ptr(), buf.as_mut_ptr().cast(), buf.len())
    };
    if n < 0 {
        errno_name(&std::io::Error::last_os_error())
    } else {
        "read".to_owned()
    }
}

fn errno_name(e: &std::io::Error) -> String {
    let name = match e.raw_os_error() {
        Some(libc::EPERM) => "EPERM",
        Some(libc::EACCES) => "EACCES",
        Some(libc::EROFS) => "EROFS",
        Some(libc::ESTALE) => "ESTALE",
        Some(libc::EINVAL) => "EINVAL",
        Some(libc::EISDIR) => "EISDIR",
        Some(libc::ENOTDIR) => "ENOTDIR",
        Some(libc::EBADF) => "EBADF",
        Some(libc::ELOOP) => "ELOOP",
        Some(libc::EXDEV) => "EXDEV",
        Some(libc::ENODATA) => "ENODATA",
        _ => return format!("{e}"),
    };
    name.to_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

// ---------------------------------------------------------------------------
// the helper's OpenByHandle, in the suite (`tests/vm/run.sh quick`)
// ---------------------------------------------------------------------------
//
// The suite's daemon is root, so the objects here are root's; the helper
// compares owners, whoever they are. What only the shipped unit can show —
// the helper as root without `CAP_DAC_OVERRIDE`, a user's file in a
// directory the user cannot enter — is `unit_check`, below.

/// How long the helper may take to answer one `OpenByHandle`. It opens one
/// object and reads one attribute; a helper that let its own open wait for a
/// fill would take the link's whole 30 s call timeout.
const PROMPT: Duration = Duration::from_secs(5);

fn handle_of(dir: &Path, name: &str) -> Result<FileHandle, String> {
    let dir = File::open(dir).map_err(|e| format!("cannot open {dir:?}: {e}"))?;
    FileHandle::at(&dir, std::ffi::OsStr::new(name)).map_err(|e| format!("no handle for {name}: {e}"))
}

/// `OpenByHandle` relative to `dir`, and how long the answer took.
fn ask(
    runtime: &tokio::runtime::Runtime,
    link: &HelperLink,
    dir: &Path,
    handle: &FileHandle,
) -> Result<(Result<OwnedFd, HelperError>, Duration), String> {
    let anchor = File::open(dir).map_err(|e| format!("cannot open {dir:?}: {e}"))?;
    let started = Instant::now();
    let answer = runtime.block_on(link.open_by_handle(&anchor, handle));
    Ok((answer, started.elapsed()))
}

/// A descriptor, promptly.
fn opened(answer: (Result<OwnedFd, HelperError>, Duration)) -> Result<OwnedFd, String> {
    match answer {
        (Ok(fd), took) if took < PROMPT => Ok(fd),
        (Ok(_), took) => Err(format!("OpenByHandle answered, but only after {took:?}")),
        (Err(e), took) => Err(format!("OpenByHandle was refused after {took:?}: {e}")),
    }
}

/// A refusal with this errno, promptly.
fn refused(answer: (Result<OwnedFd, HelperError>, Duration), errno: i32, what: &str) -> Result<(), String> {
    match answer {
        (Err(HelperError::Refused(e)), took) if e == errno && took < PROMPT => Ok(()),
        (Err(e), took) => Err(format!("{what}: {e} after {took:?}, not errno {errno}")),
        (Ok(fd), _) => Err(format!("{what}: the helper handed over {:?}", where_is(&fd))),
    }
}

fn where_is(fd: &OwnedFd) -> PathBuf {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap_or_default()
}

/// The descriptor is on `path`'s inode, and `/proc/self/fd` shows it there:
/// how the daemon learns where an object went.
fn is_at(fd: &OwnedFd, path: &Path) -> Result<(), String> {
    let got = fstat(fd.as_fd())?;
    let want = std::fs::symlink_metadata(path).map_err(|e| format!("{path:?}: {e}"))?;
    if (got.st_dev, got.st_ino) != (want.dev(), want.ino()) {
        return Err(format!("the descriptor is not on {path:?}"));
    }
    if where_is(fd) != path {
        return Err(format!("/proc/self/fd shows {:?}, not {path:?}", where_is(fd)));
    }
    Ok(())
}

fn status_flags(fd: &OwnedFd) -> Result<libc::c_int, String> {
    // SAFETY: F_GETFL on a live descriptor.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(format!("F_GETFL: {}", std::io::Error::last_os_error()));
    }
    Ok(flags)
}

fn read_only_non_blocking(fd: &OwnedFd) -> Result<(), String> {
    let flags = status_flags(fd)?;
    if flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_NONBLOCK == 0 {
        return Err(format!("the file came back with flags {flags:#o}, not O_RDONLY | O_NONBLOCK"));
    }
    Ok(())
}

/// A placeholder moved out of the folder is found by the handle it had
/// inside, where it is now; asking fills nothing; `MarkFile` with the
/// descriptor keeps it intercepted; the daemon can write to it; and once it
/// is deleted it is `ESTALE`, held open or not.
pub(crate) fn moved_out_placeholder(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let payload = b"LEFT THE FOLDER";
    let path = ctx.place("obh-out/leaving.bin", "ITEM_OBH_LEAVING", payload)?;
    let handle = handle_of(path.parent().unwrap(), "leaving.bin")?;
    let moved = ctx.outside.join("obh-leaving.bin");
    let _ = std::fs::remove_file(&moved);
    std::fs::rename(&path, &moved).map_err(|e| e.to_string())?;
    let ino = ctx.ino_of(&moved)?;
    let link = ctx.link()?;
    let before = ctx.fetches();

    let object = opened(ask(&ctx.runtime, &link, &ctx.root, &handle)?)?;
    is_at(&object, &moved)?;
    read_only_non_blocking(&object)?;
    if ctx.fetches() != before || ctx.state_of(&moved)? != Some(State::OnlineOnly) {
        return Err("asking for the descriptor filled the placeholder".into());
    }

    let file = File::from(object.try_clone().map_err(|e| e.to_string())?);
    ctx.runtime.block_on(link.mark_file(&file)).map_err(|e| format!("MarkFile with it failed: {e}"))?;
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("MarkFile with the descriptor placed no mark".into());
    }
    // Asked again, as after a daemon restart: the file now carries a mark
    // of its own, so the helper's own open is an event aimed at itself.
    let again = opened(ask(&ctx.runtime, &link, &ctx.root, &handle)?)?;
    drop(again);
    let writable = reopen_for_writing(&object).map_err(|e| format!("cannot reopen it for writing: {e}"))?;
    drop(writable);
    if ctx.fetches() != before {
        return Err("the helper's own open, or the daemon's reopen, filled the placeholder".into());
    }

    let content = ctx.read(&moved)?;
    if content != payload {
        return Err(format!("a reader of the re-marked file got {content:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch for the reader, saw {}", ctx.fetches() - before));
    }

    std::fs::remove_file(&moved).map_err(|e| e.to_string())?;
    refused(ask(&ctx.runtime, &link, &ctx.root, &handle)?, libc::ESTALE, "deleted, still held open here")?;
    drop(file);
    drop(object);
    refused(ask(&ctx.runtime, &link, &ctx.root, &handle)?, libc::ESTALE, "deleted and closed")?;
    checks.note(ctx.fs, "OpenByHandle", "a moved-out placeholder: found, re-marked, filled on open, then ESTALE");
    Ok(())
}

/// A directory moved out of the folder is found by its handle; its mark went
/// with it, and `UnmarkDir` with the descriptor takes it off.
pub(crate) fn moved_out_directory(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let inner = ctx.place("obh-dir/inner.bin", "ITEM_OBH_INNER", b"INNER")?;
    let dir = inner.parent().unwrap().to_path_buf();
    xattr::set(&dir, XATTR_ITEM_ID, b"ITEM_OBH_DIR").map_err(|e| e.to_string())?;
    let handle = handle_of(&ctx.root, "obh-dir")?;
    let moved = ctx.outside.join("obh-dir");
    let _ = std::fs::remove_dir_all(&moved);
    std::fs::rename(&dir, &moved).map_err(|e| e.to_string())?;
    let ino = ctx.ino_of(&moved)?;
    let link = ctx.link()?;

    let object = opened(ask(&ctx.runtime, &link, &ctx.root, &handle)?)?;
    is_at(&object, &moved)?;
    if fstat(object.as_fd())?.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err("the directory's descriptor is not a directory's".into());
    }
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("the directory's mark did not travel with it".into());
    }
    ctx.runtime
        .block_on(link.unmark_dir(&File::from(object)))
        .map_err(|e| format!("UnmarkDir with the descriptor failed: {e}"))?;
    if dir_mark_present(ctx.helper_pid(), ino) {
        return Err("UnmarkDir left the moved-out directory marked".into());
    }
    Ok(())
}

/// Every refusal: another uid's object, one without the attribute, one on
/// another device than the directory passed (a Btrfs subvolume) or another
/// filesystem, a directory on no root's device, and malformed handles. A
/// control of the same kind passes, so each refusal is about its one check.
pub(crate) fn refusals(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let link = ctx.link()?;
    let place = |dir: &Path, name: &str| -> Result<PathBuf, String> {
        let _ = std::fs::remove_file(dir.join(name));
        let parent = File::open(dir).map_err(|e| e.to_string())?;
        create_placeholder(&parent, name, &format!("ITEM_{name}"), 5, std::time::SystemTime::UNIX_EPOCH)
            .map_err(|e| format!("cannot place {name}: {e}"))?;
        Ok(dir.join(name))
    };
    let ask_for = |dir: &Path, name: &str| -> Result<(Result<OwnedFd, HelperError>, Duration), String> {
        ask(&ctx.runtime, &link, &ctx.root, &handle_of(dir, name)?)
    };

    let control = place(&ctx.outside, "obh-mine.bin")?;
    is_at(&opened(ask_for(&ctx.outside, "obh-mine.bin")?)?, &control)?;

    let theirs = place(&ctx.outside, "obh-theirs.bin")?;
    std::os::unix::fs::chown(&theirs, Some(crate::HOSTILE_UID), Some(crate::HOSTILE_UID)).map_err(|e| e.to_string())?;
    refused(ask_for(&ctx.outside, "obh-theirs.bin")?, libc::EPERM, "another uid's object")?;

    let plain = ctx.outside.join("obh-plain.bin");
    std::fs::write(&plain, b"plain").map_err(|e| e.to_string())?;
    refused(ask_for(&ctx.outside, "obh-plain.bin")?, libc::EPERM, "an object without the attribute")?;

    let not_a_root = Path::new("/run");
    let anchor = ask(&ctx.runtime, &link, not_a_root, &handle_of(&ctx.outside, "obh-mine.bin")?)?;
    refused(anchor, libc::EPERM, "a directory on no root's device")?;

    for (bytes, what) in [(Vec::new(), "an empty handle"), (vec![0; 129], "a 129-byte handle")] {
        refused(ask(&ctx.runtime, &link, &ctx.root, &FileHandle { kind: 1, bytes })?, libc::EINVAL, what)?;
    }
    refused(ask(&ctx.runtime, &link, &ctx.root, &FileHandle { kind: -1, bytes: vec![0; 8] })?, libc::EINVAL, "a negative type")?;

    // Another filesystem: a tmpfs file's handle means nothing to Btrfs.
    let on_tmpfs = Path::new("/run/obh-elsewhere");
    let _ = std::fs::remove_dir_all(on_tmpfs);
    std::fs::create_dir_all(on_tmpfs).map_err(|e| e.to_string())?;
    std::fs::write(on_tmpfs.join("f"), b"elsewhere").map_err(|e| e.to_string())?;
    match ask_for(on_tmpfs, "f")? {
        (Err(HelperError::Refused(libc::ESTALE | libc::EPERM)), _) => {}
        (other, _) => return Err(format!("a handle from another filesystem: {:?}", other.map(|fd| where_is(&fd)))),
    }

    // The same filesystem, another device: a Btrfs subvolume inside the
    // folder's filesystem. The kernel opens it; the helper must not.
    if ctx.fs == "btrfs" {
        let vol = ctx.outside.join("obh-vol");
        if !vol.exists() {
            let made = Command::new("btrfs")
                .args(["-q", "subvolume", "create"])
                .arg(&vol)
                .status()
                .map_err(|e| format!("cannot run btrfs: {e}"))?;
            if !made.success() {
                return Err(format!("btrfs subvolume create failed: {made}"));
            }
        }
        place(&vol, "f")?;
        refused(ask_for(&vol, "f")?, libc::EPERM, "an object in another Btrfs subvolume")?;
    } else {
        checks.note(ctx.fs, "OpenByHandle", "no subvolumes on this filesystem; the other-device case is Btrfs only");
    }
    Ok(())
}

/// A placeholder still inside the folder, in a marked directory: the
/// helper's own open of it raises an event aimed at the helper itself, which
/// must be let through at once — neither filled (the daemon never asked)
/// nor left waiting on a connection thread that is itself in the open.
pub(crate) fn own_open_exempt(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("obh-self/inside.bin", "ITEM_OBH_INSIDE", b"INSIDE")?;
    let handle = handle_of(path.parent().unwrap(), "inside.bin")?;
    let link = ctx.link()?;
    let before = ctx.fetches();
    let object = opened(ask(&ctx.runtime, &link, &ctx.root, &handle)?)?;
    is_at(&object, &path)?;
    if ctx.fetches() != before || ctx.state_of(&path)? != Some(State::OnlineOnly) {
        return Err("the helper's own open was sent to the daemon to be filled".into());
    }
    if ignore_mark_present(ctx.helper_pid(), ctx.ino_of(&path)?) {
        return Err("the helper's own open put an ignore mark on a placeholder".into());
    }
    drop(object);
    // And the file is still intercepted for everyone else.
    if ctx.read(&path)? != b"INSIDE" || ctx.fetches() != before + 1 {
        return Err("the placeholder was not filled on a reader's open afterwards".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the helper's OpenByHandle under the shipped unit (`tests/vm/run.sh unit`)
// ---------------------------------------------------------------------------

/// The daemon's side, as uid 1000 with root as the saved uid (see unit.rs):
/// a placeholder of the user's, moved by root into a directory the user
/// cannot enter, is handed over read-only, re-marked, and reopened for
/// writing by its owner; another uid's object is refused.
pub(crate) fn unit_check(
    runtime: &tokio::runtime::Runtime,
    link: &HelperLink,
    helper_pid: u32,
    folder: &Path,
    checks: &mut Checks,
) -> Result<(), String> {
    const LABEL: &str = "unit";
    let parent = File::open(folder).map_err(|e| e.to_string())?;
    create_placeholder(&parent, "obh.bin", "OBH", 64, std::time::SystemTime::UNIX_EPOCH)
        .map_err(|e| format!("cannot place obh.bin: {e}"))?;
    let handle = handle_of(folder, "obh.bin")?;
    // Outside the folders' base, whose entries unit.rs takes for folders
    // an earlier pass registered; on the same filesystem.
    let private = Path::new("/mnt/btrfs").join(format!(
        "obh-private-{}",
        folder.file_name().unwrap_or_default().to_string_lossy()
    ));
    let moved = private.join("obh.bin");
    crate::unit::as_root(|| -> Result<(), String> {
        std::fs::create_dir(&private).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        std::fs::rename(folder.join("obh.bin"), &moved).map_err(|e| e.to_string())
    })?;
    let unreachable = match File::open(&moved) {
        Err(e) if e.raw_os_error() == Some(libc::EACCES) => Ok(()),
        other => Err(format!("the user could open it by path: {other:?}")),
    };
    checks.record(LABEL, "OpenByHandle: the placeholder now sits where its owner cannot go", unreachable);

    let object = opened(ask(runtime, link, folder, &handle)?);
    let object = match object {
        Ok(object) => object,
        Err(why) => {
            checks.record(LABEL, "OpenByHandle through the unit's helper hands over the moved-out placeholder", Err(why));
            return Ok(());
        }
    };
    checks.record(
        LABEL,
        "OpenByHandle through the unit's helper hands over the moved-out placeholder, read-only, and says where it is",
        read_only_non_blocking(&object).and_then(|()| {
            if where_is(&object) == moved {
                Ok(())
            } else {
                Err(format!("/proc/self/fd shows {:?}", where_is(&object)))
            }
        }),
    );

    let file = File::from(object.try_clone().map_err(|e| e.to_string())?);
    let marked = runtime.block_on(link.mark_file(&file)).map_err(|e| format!("MarkFile: {e}")).and_then(|()| {
        let ino = fstat(object.as_fd())?.st_ino;
        if crate::unit::as_root(|| dir_mark_present(helper_pid, ino)) {
            Ok(())
        } else {
            Err("fdinfo shows no mark".into())
        }
    });
    checks.record(LABEL, "OpenByHandle: MarkFile with the descriptor re-marks it", marked);

    let written = reopen_for_writing(&object).map_err(|e| format!("cannot reopen it for writing: {e}")).and_then(|file| {
        let wrote = std::os::unix::fs::FileExt::write_at(&file, b"by its owner", 0).map_err(|e| e.to_string())?;
        let mut back = [0u8; 12];
        std::os::unix::fs::FileExt::read_at(&File::from(object.try_clone().map_err(|e| e.to_string())?), &mut back, 0)
            .map_err(|e| e.to_string())?;
        if wrote == 12 && &back == b"by its owner" {
            Ok(())
        } else {
            Err(format!("wrote {wrote} bytes, read back {back:?}"))
        }
    });
    checks.record(LABEL, "OpenByHandle: the owner reopens the descriptor for writing, and the write lands", written);

    create_placeholder(&parent, "theirs.bin", "THEIRS", 64, std::time::SystemTime::UNIX_EPOCH)
        .map_err(|e| format!("cannot place theirs.bin: {e}"))?;
    let theirs = handle_of(folder, "theirs.bin")?;
    crate::unit::as_root(|| std::os::unix::fs::chown(folder.join("theirs.bin"), Some(1001), Some(1001)))
        .map_err(|e| e.to_string())?;
    checks.record(
        LABEL,
        "OpenByHandle through the unit's helper refuses another uid's object",
        refused(ask(runtime, link, folder, &theirs)?, libc::EPERM, "another uid's object"),
    );
    Ok(())
}
