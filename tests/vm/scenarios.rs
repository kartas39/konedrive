//! End-to-end scenarios for the interception path: the real helper binary, the
//! daemon's own `sync` module, and opens driven from child processes.
//!
//! Every scenario prints `PASS`/`FAIL` on its own row; the binary exits
//! non-zero if anything failed. Run as root inside the virtme-ng VM
//! (`tests/vm/run.sh scenarios`).
//!
//! # Why the opens happen in child processes
//!
//! `konedrive-helper` exempts the owning daemon's **pid** from interception
//!: a process that holds a connection owning a registered root
//! may open that user's files without being suspended, because otherwise
//! startup recovery would ask the very daemon that is blocked to unblock
//! itself. This programme *is* the daemon — it holds the `HelperLink` — so an
//! open from any of its own threads is allowed straight through and proves
//! nothing at all. Every open that is meant to be intercepted therefore runs
//! in a separate process (`--read`, `--hold`, `--burst` below), and the
//! exemption itself becomes something to assert rather than something to
//! stumble over (see `peercred_pid_matches_event_pid`).
//!
//! # Why almost nothing here asserts on a return value
//!
//! Twice measured, the kernel reports success while doing nothing:
//! `fanotify_mark` with an ignore mask returns 0 and creates no mark when the
//! inode is open for write without `FAN_MARK_IGNORED_SURV_MODIFY`
//! (`docs/kernel-behavior-7.2.md` §2.1), and `FAN_DENY` with an errno outside
//! a specific set fails the response `write()` and leaves the opener
//! suspended forever (§5). So the assertions here are on observable state:
//! `/proc/<helper>/fdinfo` for whether a mark exists, block counts for
//! whether a file holds data, and what a reader in another process actually
//! gets back.

mod graph;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use konedrive_fs::placeholder::{
    create_placeholder, read_state, read_stamp, write_state, State, XATTR_STATE,
};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION, SOCKET_PATH};
use konedrived::sync::helper::{Clearance, HelperLink};
use konedrived::sync::root::{self, DehydrateError, SyncRoot};
use konedrived::sync::source::{ContentSource, Fetched, LocalDir, SourceError};
use konedrived::sync::{serve_hydrations, supervise_helper, InodeKey, InodeLocks, SyncError, SyncService};
use nix::sys::fanotify::{
    EventFFlags, Fanotify, FanotifyResponse, InitFlags, MarkFlags, MaskFlags, Response,
};
use nix::sys::socket::{
    connect, socket, AddressFamily, SockFlag, SockType, UnixAddr,
};
use xattr::FileExt;

/// Where the runner mounts each filesystem, and the `f_type` its superblock
/// must report. `run.sh` puts a tmpfs over `/mnt`, so a mount that silently
/// failed would leave a perfectly writable directory behind and every scenario
/// would be a scenario about tmpfs.
const FILESYSTEMS: [(&str, i64); 3] = [
    ("btrfs", 0x9123_683E),
    ("ext4", 0x0000_EF53),
    ("xfs", 0x5846_5342),
];

const ROOTS_FILE: &str = "/var/lib/konedrive/roots.json";

/// What the suite is doing right now, and since when, so a wedged run says
/// where it wedged rather than dying silently inside a VM nobody can attach
/// to.
static CURRENT: Mutex<(String, Option<Instant>)> = Mutex::new((String::new(), None));

fn now_running(what: &str) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = (what.to_owned(), Some(Instant::now()));
    println!("  .. {what}");
    let _ = std::io::stdout().flush();
}

/// Only the filesystems and scenarios named on the command line (`--fs`,
/// `--only`), so that one scenario can be re-run without the other ninety.
#[derive(Default)]
struct Filter {
    filesystems: Option<Vec<String>>,
    /// Substrings of scenario names, `|`-separated on the command line
    /// (names themselves contain commas).
    only: Option<Vec<String>>,
}

static FILTER: std::sync::OnceLock<Filter> = std::sync::OnceLock::new();

fn wanted_fs(fs: &str) -> bool {
    FILTER.get().and_then(|f| f.filesystems.as_ref()).is_none_or(|list| list.iter().any(|x| x == fs))
}

fn wanted_scenario(name: &str) -> bool {
    FILTER
        .get()
        .and_then(|f| f.only.as_ref())
        .is_none_or(|only| only.iter().any(|part| name.contains(part.as_str())))
}
/// The uid the hostile-client scenarios run as. Nothing on the machine owns
/// it; all it has is the 0666 control socket, which is the whole point.
const HOSTILE_UID: u32 = 1001;

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match argv.get(1).copied() {
        // Child modes. They must come first: a child is this same binary.
        Some("--read") => return child_read(Path::new(argv[2])),
        Some("--hold") => return child_hold(Path::new(argv[2])),
        Some("--burst") => {
            let count = argv[3].parse().unwrap();
            let expect = argv[4].parse().unwrap();
            return child_burst(Path::new(argv[2]), count, expect);
        }
        Some("--hostile") => return child_hostile(argv[2]),
        Some("--pipeline") => return child_pipeline(argv[2].parse().unwrap()),
        Some("--connections") => return child_connections(argv[2].parse().unwrap()),
        _ => {}
    }

    let mut helper = PathBuf::from("target/release/konedrive-helper");
    let mut filter = Filter::default();
    let mut measure = false;
    let mut dirs = 10_000usize;
    let mut files = 100_000usize;
    // The real account. Set only by `--graph-token`, checked after
    // the watchdog starts, below — everything above it (the filter, the
    // nofile bump) applies to this mode too. `--graph-guard` (Step 4's two
    // demonstrations, `no-hash` or `permanent-break`), `--graph-folder`
    // (required alongside `--graph-token`, and a real folder: see
    // `graph::Scope::of`), `--graph-max-bytes` (optional, defaults to
    // `graph::DEFAULT_MAX_BYTES`) and `--graph-resume-checks` (G3 and G4,
    // never run by default) only mean anything alongside `--graph-token` too.
    let mut graph_token: Option<PathBuf> = None;
    let mut graph_guard: Option<String> = None;
    let mut graph_folder: Option<String> = None;
    let mut graph_max_bytes: u64 = graph::DEFAULT_MAX_BYTES;
    let mut graph_resume_checks = false;
    let mut i = 1;
    while i < argv.len() {
        match argv[i] {
            "--helper" => {
                helper = PathBuf::from(argv[i + 1]);
                i += 1;
            }
            "--measure" => measure = true,
            "--fs" => {
                filter.filesystems = Some(argv[i + 1].split(',').map(str::to_owned).collect());
                i += 1;
            }
            "--only" => {
                filter.only = Some(argv[i + 1].split('|').map(str::to_owned).collect());
                i += 1;
            }
            "--dirs" => {
                dirs = argv[i + 1].parse().unwrap();
                i += 1;
            }
            "--files" => {
                files = argv[i + 1].parse().unwrap();
                i += 1;
            }
            "--graph-token" => {
                graph_token = Some(PathBuf::from(argv[i + 1]));
                i += 1;
            }
            "--graph-guard" => {
                graph_guard = Some(argv[i + 1].to_owned());
                i += 1;
            }
            "--graph-folder" => {
                graph_folder = Some(argv[i + 1].to_owned());
                i += 1;
            }
            "--graph-max-bytes" => {
                graph_max_bytes = argv[i + 1].parse().unwrap();
                i += 1;
            }
            "--graph-resume-checks" => graph_resume_checks = true,
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(64);
            }
        }
        i += 1;
    }

    let _ = FILTER.set(filter);
    raise_nofile();
    // A guest that hangs reports nothing at all; bail out loudly instead.
    // Measured from the start of the current step, not of the run: the whole
    // suite on three filesystems takes longer than any one budget that would
    // still catch a wedged scenario in reasonable time. Every wait inside a
    // step is bounded, so nothing legitimate comes near this.
    let budget = Duration::from_secs(if measure { 3600 } else { 900 });
    let started = Instant::now();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let (what, since) = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if since.unwrap_or(started).elapsed() > budget {
            println!("WATCHDOG: no progress for {budget:?} — the suite is wedged in: {what}");
            let _ = std::io::stdout().flush();
            std::process::exit(99);
        }
    });

    if let Some(token) = graph_token {
        let args = graph::Args { folder: graph_folder, max_bytes: graph_max_bytes, resume_checks: graph_resume_checks };
        std::process::exit(graph::graph_mode(&helper, &token, graph_guard.as_deref(), args));
    }

    if measure {
        std::process::exit(measure_mode(&helper, dirs, files));
    }

    let mut failed: Vec<String> = Vec::new();
    let mut passed = 0usize;
    let mut timings: Vec<(String, Duration)> = Vec::new();
    for (fs, magic) in FILESYSTEMS {
        if !wanted_fs(fs) {
            continue;
        }
        println!("== {fs} ==");
        match run_suite(&helper, fs, magic) {
            Ok(checks) => {
                passed += checks.passed;
                failed.extend(checks.failed);
                timings.extend(checks.timings);
            }
            Err(why) => {
                println!("  FAIL  [{fs}] the suite could not start: {why}");
                failed.push(format!("[{fs}] the suite could not start: {why}"));
            }
        }
    }

    println!();
    println!("{passed} passed, {} failed", failed.len());
    for failure in &failed {
        println!("  {failure}");
    }

    if !timings.is_empty() {
        timings.sort_by(|a, b| b.1.cmp(&a.1));
        println!();
        println!("slowest {} step(s):", timings.len().min(10));
        for (name, elapsed) in timings.iter().take(10) {
            println!("  {:>7.2} s  {name}", elapsed.as_secs_f64());
        }
    }

    std::process::exit(if failed.is_empty() { 0 } else { 1 });
}

/// The helper holds roughly 1.3 descriptors per suspended open (the event fd
/// plus the dup that travels to the daemon), and the burst scenario deliberately
/// suspends thousands at once. Left at the guest's 1024 soft limit the burst
/// would measure `RLIMIT_NOFILE`, not the pool. Root may raise the hard limit
/// too, and children — including the helper — inherit it.
fn raise_nofile() {
    let want = 1 << 20;
    let limit = libc::rlimit { rlim_cur: want, rlim_max: want };
    // SAFETY: a plain setrlimit with a live, correctly sized `rlimit`.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        eprintln!("cannot raise RLIMIT_NOFILE: {}", std::io::Error::last_os_error());
    }
}

// ---------------------------------------------------------------------------
// child modes
// ---------------------------------------------------------------------------

/// Opens a file and copies it to stdout, exiting with the errno on failure.
/// This is the only thing in the suite that performs an open meant to be
/// intercepted.
fn child_read(path: &Path) {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut content = Vec::new();
            if let Err(e) = file.read_to_end(&mut content) {
                std::process::exit(e.raw_os_error().unwrap_or(5));
            }
            let _ = std::io::stdout().write_all(&content);
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        Err(e) => std::process::exit(e.raw_os_error().unwrap_or(5)),
    }
}

/// Opens a file, says so, and keeps the descriptor until stdin closes. A
/// second descriptor on a file is what `F_SETLEASE` refuses, which is how
/// "dehydrate while open is refused" is driven from outside this process.
fn child_hold(path: &Path) {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            println!("error {}", e.raw_os_error().unwrap_or(5));
            let _ = std::io::stdout().flush();
            std::process::exit(e.raw_os_error().unwrap_or(5));
        }
    };
    println!("held");
    let _ = std::io::stdout().flush();
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(file);
    std::process::exit(0);
}

/// Opens `dir/burst-<i>` for every `i` below `count`, all at once, one thread
/// each, and reports what every one of them got. One process rather than
/// `count` processes: several thousand of those would measure the guest's
/// memory, and what is under test is the helper's bounded pool.
fn child_burst(dir: &Path, count: usize, expect: u8) {
    let (tx, rx) = mpsc::channel::<(usize, Result<usize, i32>, u64)>();
    let mut handles = Vec::with_capacity(count);
    for i in 0..count {
        let path = dir.join(format!("burst-{i}"));
        let tx = tx.clone();
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || {
                let started = Instant::now();
                let outcome = match std::fs::File::open(&path) {
                    Ok(mut file) => {
                        let mut content = Vec::new();
                        match file.read_to_end(&mut content) {
                            // `wrong` counts the bytes that are not what the
                            // payload says they should be: an opener that was
                            // let through onto an unfilled placeholder reads
                            // zeros, and that is the outcome worth counting
                            // separately from a clean failure.
                            Ok(_) => Ok(content.iter().filter(|b| **b != expect).count()),
                            Err(e) => Err(e.raw_os_error().unwrap_or(5)),
                        }
                    }
                    Err(e) => Err(e.raw_os_error().unwrap_or(5)),
                };
                let _ = tx.send((i, outcome, started.elapsed().as_millis() as u64));
            })
            .expect("a burst thread");
        handles.push(handle);
    }
    drop(tx);

    let mut ok = 0usize;
    let mut wrong = 0usize;
    let mut errors: BTreeMap<i32, usize> = BTreeMap::new();
    let mut answered = 0usize;
    while let Ok((i, outcome, waited)) = rx.recv() {
        answered += 1;
        match outcome {
            Ok(0) => {
                ok += 1;
                println!("TOOK {i} {waited}");
            }
            Ok(_) => {
                wrong += 1;
                println!("WRONG {i} {waited}");
            }
            Err(errno) => {
                *errors.entry(errno).or_default() += 1;
                println!("FAILED {i} {errno} {waited}");
            }
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    let errs: Vec<String> = errors.iter().map(|(e, n)| format!("{e}:{n}")).collect();
    println!("BURST answered={answered} ok={ok} wrong={wrong} errs={}", errs.join(","));
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// A second, unprivileged uid with nothing but the 0666 control socket. It
/// tries the three things that would be catastrophic if they worked, and
/// prints what the helper answered. `Command::uid` put it here; nothing in
/// this process is trusted by the helper.
fn child_hostile(root_id: &str) {
    let mut channel = match raw_connect() {
        Ok(channel) => channel,
        Err(e) => {
            println!("CONNECT-FAILED {e}");
            std::process::exit(1);
        }
    };
    // The helper greets unprompted.
    match channel.recv::<ToDaemon>() {
        Ok((ToDaemon::Welcome { version }, _)) if version == PROTOCOL_VERSION => {}
        other => {
            println!("NO-WELCOME {other:?}");
            std::process::exit(1);
        }
    }

    // 1. Force-allow somebody else's suspended opens by guessing request ids.
    //    A request id is a small sequential integer, so guessing is trivial;
    //    what must stop it is the job's recorded owner.
    let mut acks = Vec::new();
    for req_id in 1..=64u64 {
        if channel.send(&ToHelper::HydrateDone { req_id, errno: 0 }, None).is_err() {
            break;
        }
        match channel.recv::<ToDaemon>() {
            Ok((ToDaemon::Ack { errno }, _)) => acks.push(errno),
            _ => break,
        }
    }
    println!("HYDRATEDONE-ACKS {}", acks.len());

    // 2. Unregister the victim's root by id.
    let _ = channel.send(&ToHelper::UnregisterRoot { root_id: root_id.to_owned() }, None);
    println!("UNREGISTER {}", ack_of(&mut channel));

    // 3. Take the mark off the victim's root directory. The directory is
    //    world-readable, and opening a directory raises no event, so getting
    //    the descriptor costs nothing — only the helper's own authorisation
    //    stands between this and every placeholder in that tree becoming
    //    uninterceptable.
    let victim_root = std::env::var("KONEDRIVE_VICTIM_ROOT").unwrap_or_default();
    match std::fs::File::open(&victim_root) {
        Ok(dir) => {
            let _ = channel.send(&ToHelper::UnmarkDir, Some(dir.as_fd()));
            println!("UNMARKDIR {}", ack_of(&mut channel));
        }
        Err(e) => println!("UNMARKDIR open-failed {e}"),
    }
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// Sends `count` requests before reading a single reply, then reads them all,
/// then asks once more. What it prints is what the helper did to a peer that
/// is slow to read its `Ack`s: `ACKS <n>` is how many replies
/// arrived before the connection ended or the count was reached, and the last
/// line says whether the connection was still there afterwards.
///
/// The requests are `Hello`s, which the helper answers from its own state and
/// which touch nothing, so any uid may send them and none of them changes
/// anything but the socket.
fn child_pipeline(count: usize) {
    let mut writer = match raw_connect() {
        Ok(channel) => channel,
        Err(e) => {
            println!("CONNECT-FAILED {e}");
            std::process::exit(1);
        }
    };
    let mut reader = match writer.get_ref().try_clone().map(Channel::new) {
        Ok(Ok(channel)) => channel,
        _ => {
            println!("CONNECT-FAILED cannot split the connection");
            std::process::exit(1);
        }
    };
    match reader.recv::<ToDaemon>() {
        Ok((ToDaemon::Welcome { .. }, _)) => {}
        other => {
            println!("NO-WELCOME {other:?}");
            std::process::exit(1);
        }
    }
    let sender = std::thread::spawn(move || {
        let mut sent = 0usize;
        for _ in 0..count {
            if writer.send(&ToHelper::Hello { version: PROTOCOL_VERSION }, None).is_err() {
                break;
            }
            sent += 1;
        }
        (sent, writer)
    });
    // Long enough for every buffer between the two ends to fill: the helper's
    // outbox, the socket in both directions, and whatever the helper's reader
    // is holding.
    std::thread::sleep(Duration::from_secs(2));
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(10)));
    let mut acks = 0usize;
    let mut refused = 0usize;
    while acks < count {
        match reader.recv::<ToDaemon>() {
            Ok((ToDaemon::Ack { errno }, _)) => {
                acks += 1;
                if errno != 0 {
                    refused += 1;
                }
            }
            Ok((other, _)) => {
                println!("UNEXPECTED {other:?}");
                break;
            }
            Err(e) => {
                println!("ENDED {e}");
                break;
            }
        }
    }
    let (sent, mut writer) = sender.join().expect("the sender thread");
    println!("SENT {sent}");
    println!("ACKS {acks} REFUSED {refused}");
    let alive = writer.send(&ToHelper::Hello { version: PROTOCOL_VERSION }, None).is_ok()
        && matches!(reader.recv::<ToDaemon>(), Ok((ToDaemon::Ack { errno: 0 }, _)));
    println!("{}", if alive { "ALIVE" } else { "DEAD" });
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// Opens `count` connections to the helper, one after another, keeping every
/// one of them open, and prints how many were greeted. A connection the
/// helper refuses is closed without a `Welcome`.
fn child_connections(count: usize) {
    let mut held = Vec::new();
    let mut greeted = 0usize;
    for _ in 0..count {
        let Ok(mut channel) = raw_connect() else { continue };
        let _ = channel.get_ref().set_read_timeout(Some(Duration::from_secs(2)));
        if let Ok((ToDaemon::Welcome { .. }, _)) = channel.recv::<ToDaemon>() {
            greeted += 1;
        }
        held.push(channel);
    }
    println!("GREETED {greeted} OF {count}");
    let _ = std::io::stdout().flush();
    drop(held);
    std::process::exit(0);
}

fn ack_of(channel: &mut Channel) -> String {
    match channel.recv::<ToDaemon>() {
        Ok((ToDaemon::Ack { errno }, _)) => format!("errno={errno}"),
        Ok((other, _)) => format!("unexpected={other:?}"),
        Err(e) => format!("no-ack={e}"),
    }
}

fn raw_connect() -> std::io::Result<Channel> {
    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
    let addr = UnixAddr::new(Path::new(SOCKET_PATH))?;
    connect(std::os::fd::AsRawFd::as_raw_fd(&fd), &addr)?;
    Channel::new(UnixStream::from(fd))
}

// ---------------------------------------------------------------------------
// the check ledger
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Checks {
    passed: usize,
    failed: Vec<String>,
    /// One entry per scenario recorded through `record_timed`, so the run can
    /// end with the slowest steps rather than only pass/fail counts. Kernel
    /// facts and the "helper still running" check go through plain `record`
    /// and never appear here — they are not scenarios with a wait budget of
    /// their own.
    timings: Vec<(String, Duration)>,
}

impl Checks {
    fn record(&mut self, fs: &str, name: &str, outcome: Result<(), String>) {
        self.record_timed(fs, name, outcome, None);
    }

    /// Same as `record`, but appends each `ok`/`FAIL` line with how long the
    /// step took (e.g. `ok    [btrfs] name (1.84 s)`) and, when `elapsed` is
    /// given, remembers it for the end-of-run slowest-steps list.
    fn record_timed(
        &mut self,
        fs: &str,
        name: &str,
        outcome: Result<(), String>,
        elapsed: Option<Duration>,
    ) {
        let suffix =
            elapsed.map(|e| format!(" ({:.2} s)", e.as_secs_f64())).unwrap_or_default();
        match outcome {
            Ok(()) => {
                println!("  ok    [{fs}] {name}{suffix}");
                self.passed += 1;
            }
            Err(why) => {
                println!("  FAIL  [{fs}] {name}{suffix}: {why}");
                self.failed.push(format!("[{fs}] {name}: {why}"));
            }
        }
        if let Some(elapsed) = elapsed {
            self.timings.push((format!("[{fs}] {name}"), elapsed));
        }
        let _ = std::io::stdout().flush();
    }

    fn note(&mut self, fs: &str, name: &str, what: &str) {
        println!("  note  [{fs}] {name}: {what}");
        let _ = std::io::stdout().flush();
    }
}

// ---------------------------------------------------------------------------
// what the helper's fanotify group actually holds
// ---------------------------------------------------------------------------

/// One inode mark, as `/proc/<helper>/fdinfo/<group>` reports it. This is the
/// only honest source of truth for whether a mark exists: `fanotify_mark`
/// returns 0 for a mark it did not create (§2.1).
#[derive(Debug, Clone, Copy)]
struct Mark {
    ino: u64,
    mask: u64,
    ignored_mask: u64,
    mflags: u64,
}

fn helper_marks(pid: u32) -> Vec<Mark> {
    let mut marks = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
        return marks;
    };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        for line in text.lines() {
            if let Some(mark) = parse_mark(line) {
                marks.push(mark);
            }
        }
    }
    marks
}

fn parse_mark(line: &str) -> Option<Mark> {
    let rest = line.strip_prefix("fanotify ")?;
    let mut fields: BTreeMap<&str, u64> = BTreeMap::new();
    for token in rest.split_whitespace() {
        // `f_handle:` carries an opaque blob far wider than a u64; every field
        // that cannot be read as hex is simply not one of the four below.
        if let Some((key, value)) = token.split_once(':') {
            if let Ok(parsed) = u64::from_str_radix(value, 16) {
                fields.insert(key, parsed);
            }
        }
    }
    Some(Mark {
        ino: *fields.get("ino")?,
        mask: fields.get("mask").copied().unwrap_or(0),
        ignored_mask: fields.get("ignored_mask").copied().unwrap_or(0),
        mflags: fields.get("mflags").copied().unwrap_or(0),
    })
}

const FAN_OPEN_PERM: u64 = 0x0001_0000;

fn ignore_mark_present(pid: u32, ino: u64) -> bool {
    helper_marks(pid)
        .iter()
        .any(|m| m.ino == ino && m.ignored_mask & FAN_OPEN_PERM != 0)
}

fn dir_mark_present(pid: u32, ino: u64) -> bool {
    helper_marks(pid).iter().any(|m| m.ino == ino && m.mask & FAN_OPEN_PERM != 0)
}


// ---------------------------------------------------------------------------
// kernel facts the helper's design rests on, measured directly
// ---------------------------------------------------------------------------
//
// These need no helper and no daemon: they are the two results
// `docs/kernel-behavior-7.2.md` §10 listed as having no committed programme at
// all, both of them load-bearing. They run against a fanotify group of this
// process's own, in a directory nobody else has marked, before the helper for
// this filesystem is started.

/// A fanotify group set up exactly as `konedrive_helper::marks::Marks` sets
/// one up, minus the unlimited-queue and unlimited-mark flags nothing here
/// needs.
fn own_group() -> Result<Fanotify, String> {
    Fanotify::init(
        InitFlags::FAN_CLASS_PRE_CONTENT | InitFlags::FAN_CLOEXEC | InitFlags::FAN_NONBLOCK,
        EventFFlags::O_RDWR | EventFFlags::O_LARGEFILE | EventFFlags::O_CLOEXEC,
    )
    .map_err(|e| format!("cannot create a fanotify group: {e}"))
}

fn next_own_event(
    group: &Fanotify,
    within: Duration,
) -> Option<nix::sys::fanotify::FanotifyEvent> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match group.read_events() {
            Ok(events) => {
                if let Some(event) = events.into_iter().next() {
                    return Some(event);
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(_) => return None,
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Opens a file on another thread, so this one can answer the permission event
/// that open raises. fanotify does not exempt the listening process, so a
/// single-threaded version of this deadlocks.
struct ThreadOpener {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ThreadOpener {
    fn start(path: PathBuf, write: bool) -> (Self, Arc<Mutex<Option<File>>>) {
        let done = Arc::new(AtomicBool::new(false));
        let held: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
        let flag = Arc::clone(&done);
        let slot = Arc::clone(&held);
        let handle = std::thread::spawn(move || {
            let opened = File::options().read(true).write(write).open(&path);
            if let Ok(file) = opened {
                *slot.lock().unwrap() = Some(file);
            }
            flag.store(true, Ordering::SeqCst);
        });
        (ThreadOpener { done, handle: Some(handle) }, held)
    }

    fn finished(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.done.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Waits for the opener only if it has already returned. A thread still
    /// suspended in `open()` is released when the group's descriptor closes
    /// (`fanotify(7)`), which happens when the caller drops the group — so
    /// joining unconditionally is how a failed measurement becomes a hung
    /// programme.
    fn join(mut self) {
        if let Some(handle) = self.handle.take() {
            if self.done.load(Ordering::SeqCst) {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for ThreadOpener {
    fn drop(&mut self) {
        // Detached deliberately: see `join`.
        self.handle.take();
    }
}

fn own_marks(group: &Fanotify) -> Vec<Mark> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(&group.as_fd());
    std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .unwrap_or_default()
        .lines()
        .filter_map(parse_mark)
        .collect()
}

/// `docs/kernel-behavior-7.2.md` §5.1, which had no committed programme: a
/// permission response is matched against the descriptor **number** the kernel
/// handed out, not against the open file description behind it.
///
/// Both halves matter to the helper. The first is why `take_fd` keeps the
/// exact descriptor through the whole "ask the daemon and wait" path instead
/// of a convenient duplicate. The second is the dangerous one: a closed
/// number is immediately reusable, so answering a remembered number after
/// closing it can answer somebody else's event.
fn response_matched_by_number(dir: &Path) -> Result<(), String> {
    // Created before the mark: a file created *inside* a marked directory
    // raises a permission event aimed at this process, and the thread that
    // would have to answer it is the one blocked in `open()` (kernel fact 7).
    let path = dir.join("numbered.bin");
    std::fs::write(&path, b"numbered").map_err(|e| e.to_string())?;

    let group = own_group()?;
    let handle = File::open(dir).map_err(|e| e.to_string())?;
    group
        .mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            handle.as_fd(),
            None::<&Path>,
        )
        .map_err(|e| format!("cannot mark the directory: {e}"))?;

    let (opener, _held) = ThreadOpener::start(path.clone(), false);
    let event = next_own_event(&group, Duration::from_secs(5))
        .ok_or("the open raised no permission event")?;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&event.fd().ok_or("no descriptor on the event")?);
    std::mem::forget(event);

    // A duplicate has the same open file description and a different number.
    // SAFETY: `raw` is an open descriptor this process owns.
    let duplicate = unsafe { libc::dup(raw) };
    if duplicate < 0 {
        return Err(format!("cannot duplicate the event fd: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `duplicate` was just returned by `dup` and is owned here.
    let duplicate = unsafe { OwnedFd::from_raw_fd_checked(duplicate) };
    let by_duplicate = group
        .write_response(FanotifyResponse::new(duplicate.as_fd(), Response::FAN_ALLOW));
    match by_duplicate {
        Err(nix::errno::Errno::ENOENT) => {}
        Err(other) => {
            return Err(format!(
                "answering with a duplicate failed with {other}, not the ENOENT §5.1 records"
            ))
        }
        Ok(()) => {
            // The opener has been released; there is nothing left to measure
            // and the finding itself is the important part.
            let _ = opener.finished(Duration::from_secs(5));
            opener.join();
            return Err(
                "a duplicate of the event fd ANSWERED the event: the helper's rule that an event \
                 must be answered with its own descriptor no longer holds"
                    .into(),
            );
        }
    }
    if opener.finished(Duration::from_millis(300)) {
        return Err("the opener was released by a response that reported failure".into());
    }

    // Now the second half: close the number, then answer with it anyway.
    // SAFETY: closing a descriptor this process owns and will not use again
    // through this path.
    unsafe { libc::close(raw) };
    // SAFETY: `raw` is used only as the number the response names, which is
    // exactly what is under test; nothing reads or writes through it.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    let after_close = group.write_response(FanotifyResponse::new(borrowed, Response::FAN_ALLOW));
    let released = opener.finished(Duration::from_secs(5));
    // Closing the group is what releases an opener whose event nothing
    // answered, so it happens before the join and before anything returns.
    drop(group);
    let _ = opener.finished(Duration::from_secs(5));
    opener.join();
    drop(duplicate);
    match (after_close, released) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => {
            Err("the response naming a closed number succeeded but the opener stayed blocked"
                .into())
        }
        (Err(e), _) => Err(format!(
            "answering with the original number after closing it failed with {e}; §5.1 says it \
             succeeds, and a number that can be recycled while still answerable is what makes \
             `take_fd`'s ownership rule load-bearing"
        )),
    }
}

/// `docs/kernel-behavior-7.2.md` §2.1's decisive row, which had no committed
/// programme: an ignore mark without `FAN_MARK_IGNORED_SURV_MODIFY` is
/// silently refused whenever **anybody** holds the inode open for writing —
/// no event fd is involved. That is what proves the refusal comes from
/// `inode_is_open_for_write()` rather than from anything fanotify-specific,
/// and therefore that `SURV_MODIFY` is not optional for a helper whose event
/// fds are always `O_RDWR`.
fn ignore_without_surv_modify(dir: &Path) -> Result<(), String> {
    // Created before the mark, for the reason in `response_matched_by_number`.
    let name = "surv-modify.bin";
    let path = dir.join(name);
    std::fs::write(&path, b"x").map_err(|e| e.to_string())?;
    let ino = std::fs::metadata(&path).map_err(|e| e.to_string())?.ino();

    let group = own_group()?;
    let handle = File::open(dir).map_err(|e| e.to_string())?;
    group
        .mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            handle.as_fd(),
            None::<&Path>,
        )
        .map_err(|e| format!("cannot mark the directory: {e}"))?;
    let present = |group: &Fanotify| {
        own_marks(group).iter().any(|m| m.ino == ino && m.ignored_mask & FAN_OPEN_PERM != 0)
    };
    // Always by (dirfd, name): resolving a name raises no event, while
    // opening the file would raise one aimed at this very process. Path
    // resolution is therefore held constant and the only variable is what
    // descriptor happens to be open.
    let plain =
        MarkFlags::FAN_MARK_ADD | MarkFlags::FAN_MARK_IGNORE | MarkFlags::FAN_MARK_EVICTABLE;
    let add = |flags: MarkFlags| {
        group.mark(flags, MaskFlags::FAN_OPEN_PERM, handle.as_fd(), Some(Path::new(name)))
    };
    let remove = || {
        let _ = group.mark(
            MarkFlags::FAN_MARK_REMOVE | MarkFlags::FAN_MARK_IGNORE,
            MaskFlags::FAN_OPEN_PERM,
            handle.as_fd(),
            Some(Path::new(name)),
        );
    };

    // The control: with nothing open on the inode, the plain ignore mark
    // lands. Without this row the row below would only show that something
    // was wrong, not what.
    add(plain).map_err(|e| format!("the plain ignore mark was refused outright: {e}"))?;
    if !present(&group) {
        return Err("a plain ignore mark on an idle file did not appear in fdinfo either".into());
    }
    remove();
    if present(&group) {
        return Err("the ignore mark could not be removed again".into());
    }

    // The decisive row: an ordinary `O_RDWR` descriptor, opened by another
    // thread of this process and answered from here, with no event fd
    // anywhere near the mark.
    let (opener, held) = ThreadOpener::start(path.clone(), true);
    let event = next_own_event(&group, Duration::from_secs(5))
        .ok_or("the writable open raised no permission event")?;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&event.fd().ok_or("no descriptor on the event")?);
    std::mem::forget(event);
    // SAFETY: `raw` is the descriptor the kernel handed out for this event;
    // it is answered and closed exactly once here.
    let owned = unsafe { OwnedFd::from_raw_fd_checked(raw) };
    group
        .write_response(FanotifyResponse::new(owned.as_fd(), Response::FAN_ALLOW))
        .map_err(|e| format!("cannot allow the writable open: {e}"))?;
    drop(owned);
    if !opener.finished(Duration::from_secs(5)) {
        return Err("the writable opener never returned".into());
    }
    let writable = held.lock().unwrap().is_some();
    if !writable {
        return Err("the writable open failed, so nothing holds the inode open for write".into());
    }

    let refused_silently = add(plain).is_ok() && !present(&group);
    let with_surv = add(plain | MarkFlags::FAN_MARK_IGNORED_SURV_MODIFY).is_ok() && present(&group);
    remove();
    *held.lock().unwrap() = None;
    opener.join();

    if !refused_silently {
        return Err(
            "an ignore mark without FAN_MARK_IGNORED_SURV_MODIFY was NOT silently refused while \
             an ordinary O_RDWR descriptor was open: §2.1 no longer holds, and the flag's \
             justification has to be rewritten"
                .into(),
        );
    }
    if !with_surv {
        return Err(
            "FAN_MARK_IGNORED_SURV_MODIFY did not make the mark land either: the helper's ignore \
             marks do not exist at all"
                .into(),
        );
    }
    Ok(())
}

/// A small, safe-ish wrapper around `OwnedFd::from_raw_fd`, kept in one place
/// so the unsafety is spelled out once.
trait FromRawFdChecked {
    /// # Safety
    /// `raw` must be an open descriptor this process owns and which nothing
    /// else will close.
    unsafe fn from_raw_fd_checked(raw: libc::c_int) -> OwnedFd;
}

impl FromRawFdChecked for OwnedFd {
    unsafe fn from_raw_fd_checked(raw: libc::c_int) -> OwnedFd {
        use std::os::fd::FromRawFd;
        unsafe { OwnedFd::from_raw_fd(raw) }
    }
}

/// Whether a **read-only** opener suspended in a fanotify permission wait
/// already counts against a write lease.
///
/// Dehydration (`docs/design/hydration.md` §8) empties a file under a write
/// lease, on the promise that the lease
/// is refused while anybody has the file open. An opener suspended in a
/// permission wait has no descriptor yet, and one the helper lets through
/// still has to get past `break_lease()` afterwards. If the kernel counted a
/// read-only open (`i_readcount`) only once the open had completed, a
/// dehydration could take its lease in that gap — after the helper let the
/// opener through onto a `hydrated` file, before the opener's own
/// `break_lease()` — and the opener, woken when the lease went, would read the
/// punched file. The helper's own event descriptor refuses the lease while it
/// is open (it is `O_RDWR`), but it is closed the moment the answer is
/// written, before the opener runs again.
///
/// Measured with this process's own group and nobody reading its events, so
/// that no event descriptor exists: while the opener is suspended, the only
/// thing that can refuse the lease is the opener itself.
fn suspended_reader_refuses_a_lease(dir: &Path) -> Result<(), String> {
    use konedrive_fs::lease::WriteLease;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    let path = dir.join("leased.bin");
    std::fs::write(&path, b"leased").map_err(|e| e.to_string())?;
    // Opened before the mark: this process's own opens are events too.
    let file = File::options().read(true).write(true).open(&path).map_err(|e| e.to_string())?;
    match WriteLease::take(&file).map_err(|e| e.to_string())? {
        Some(lease) => drop(lease),
        None => return Err("the control failed: a lease was refused with nothing else open".into()),
    }

    let group = own_group()?;
    let handle = File::open(dir).map_err(|e| e.to_string())?;
    group
        .mark(
            MarkFlags::FAN_MARK_ADD,
            MaskFlags::FAN_OPEN_PERM | MaskFlags::FAN_EVENT_ON_CHILD,
            handle.as_fd(),
            None::<&Path>,
        )
        .map_err(|e| format!("cannot mark the directory: {e}"))?;
    let (opener, _held) = ThreadOpener::start(path.clone(), false);
    // Queued, and deliberately not read: reading would create the event's
    // own descriptor, which refuses the lease by itself.
    let mut fds = [PollFd::new(group.as_fd(), PollFlags::POLLIN)];
    let queued = matches!(poll(&mut fds, PollTimeout::from(5000u16)), Ok(n) if n > 0);
    let refused = if queued {
        match WriteLease::take(&file).map_err(|e| e.to_string()) {
            Ok(Some(lease)) => {
                drop(lease);
                Some(false)
            }
            Ok(None) => Some(true),
            Err(e) => {
                drop(group);
                opener.join();
                return Err(format!("the lease failed outright: {e}"));
            }
        }
    } else {
        None
    };
    // Closing the group releases the opener.
    drop(group);
    let released = opener.finished(Duration::from_secs(5));
    opener.join();
    match (refused, released) {
        (None, _) => Err("the read-only open raised no permission event".into()),
        (_, false) => Err("the opener was not released when the group closed".into()),
        (Some(true), true) => Ok(()),
        (Some(false), true) => Err(
            "a write lease was GRANTED while a read-only opener was suspended in a permission \
             wait: an opener the helper lets through can be overtaken by a dehydration's lease \
             and read the file it punches"
                .into(),
        ),
    }
}

fn kernel_facts(fs: &'static str, checks: &mut Checks) {
    let dir = PathBuf::from(format!("/mnt/{fs}/facts"));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        checks.record(fs, "the kernel-fact checks could run", Err(e.to_string()));
        return;
    }
    now_running("kernel fact: a response is matched by descriptor number");
    checks.record(
        fs,
        "a permission response is matched by descriptor number, not by open file description",
        response_matched_by_number(&dir),
    );
    now_running("kernel fact: an ignore mark without SURV_MODIFY is refused");
    checks.record(
        fs,
        "an ignore mark without SURV_MODIFY is silently refused while the inode is open for write",
        ignore_without_surv_modify(&dir),
    );
    now_running("kernel fact: a suspended read-only opener refuses a write lease");
    checks.record(
        fs,
        "a read-only opener suspended in a permission wait already refuses a write lease",
        suspended_reader_refuses_a_lease(&dir),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// the helper process
// ---------------------------------------------------------------------------

struct HelperProc {
    child: Child,
    binary: PathBuf,
    log: PathBuf,
    /// A hard `RLIMIT_NOFILE` for the helper alone, for the `EMFILE` scenario.
    nofile: Option<u64>,
    fault: Option<(String, String)>,
}

impl HelperProc {
    fn start(binary: &Path, log: &Path) -> Result<Self, String> {
        let mut proc = HelperProc {
            child: spawn_helper(binary, log, None, None)?,
            binary: binary.to_path_buf(),
            log: log.to_path_buf(),
            nofile: None,
            fault: None,
        };
        proc.await_socket()?;
        Ok(proc)
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(SOCKET_PATH);
    }

    /// Restarts the helper with whatever `nofile`/`fault` are currently set.
    fn restart(&mut self) -> Result<(), String> {
        self.stop();
        self.child = spawn_helper(&self.binary, &self.log, self.nofile, self.fault.clone())?;
        self.await_socket()
    }

    /// Waits for the socket file to appear — and **only** for that.
    ///
    /// It used to probe by connecting, which was a real defect in this
    /// harness and a real one in the helper. `serve_one` reads `SO_PEERCRED`,
    /// so a probe from this process is accepted as *this uid's daemon* and
    /// inserted into `shared.daemons`; when it then drops, its `Disconnect`
    /// removes the entry unless a newer connection has already replaced it.
    /// Connections are accepted on separate threads, so insert order is not
    /// accept order: the probe's thread can insert **after** the real
    /// connection has, and its cleanup then evicts a live daemon. Every open
    /// afterwards waits the full `DAEMON_WAIT` and is denied `EIO`, which is
    /// exactly the intermittent failure that appeared after a helper restart.
    ///
    /// Waiting for `bind` and letting `connect_daemon` retry the connect is
    /// enough, and raises no connection the helper can mistake for a daemon.
    /// (The helper half is fixed too — numbers connections at
    /// accept time and never lets an older one replace a newer — but a
    /// harness that does not raise the question is better than one that
    /// relies on the answer.)
    fn await_socket(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if Path::new(SOCKET_PATH).exists() {
                return Ok(());
            }
            if !self.alive() {
                return Err(format!("the helper exited during startup; log: {}", tail(&self.log)));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!("the helper never bound {SOCKET_PATH}; log: {}", tail(&self.log)))
    }
}

fn spawn_helper(
    binary: &Path,
    log: &Path,
    nofile: Option<u64>,
    fault: Option<(String, String)>,
) -> Result<Child, String> {
    let out = File::options()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut command = Command::new(binary);
    command.stdout(Stdio::from(out)).stderr(Stdio::from(err)).stdin(Stdio::null());
    if let Some((key, value)) = fault {
        command.env(key, value);
    }
    if let Some(limit) = nofile {
        // SAFETY: `setrlimit` between fork and exec is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                let rlimit = libc::rlimit { rlim_cur: limit, rlim_max: limit };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &rlimit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().map_err(|e| format!("cannot start {}: {e}", binary.display()))
}

/// How many times the helper said a particular thing. The burst scenario uses
/// it to tell the three refusal paths apart — a full worker queue, a full
/// outbox, and a full waiter budget all end in `EAGAIN` or `EIO` at the
/// opener, and only the journal says which constant was the binding one.
fn count_in_log(log: &Path, needle: &str) -> usize {
    std::fs::read_to_string(log)
        .map(|text| text.lines().filter(|line| line.contains(needle)).count())
        .unwrap_or(0)
}

/// How many times the helper did something it reports through a throttle
///. A throttled line stands for as many occurrences as it says
/// it does; any other line stands for itself, which is also what every line
/// meant before the throttle existed — so this counts correctly against a
/// helper from either side of that change.
fn occurrences_in_log(log: &Path, needle: &str) -> usize {
    std::fs::read_to_string(log)
        .map(|text| {
            text.lines().filter(|line| line.contains(needle)).map(occurrences_on).sum()
        })
        .unwrap_or(0)
}

/// The count a throttled line carries: the number in front of
/// [`THROTTLE_MARK`], or 1.
fn occurrences_on(line: &str) -> usize {
    let Some(at) = line.find(THROTTLE_MARK) else { return 1 };
    let digits: String = line[..at]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().unwrap_or(1)
}

/// What the helper's throttled lines say after their count. Spelled out here
/// rather than imported because the helper's `main.rs` is a binary.
const THROTTLE_MARK: &str = " occurrence(s) since the last line like this";

/// What the helper logs when the group was readable and its first read found
/// nothing — an event whose descriptor the kernel could not create (a leased
/// file) and answered itself. The helper's `UNOPENABLE`, in part.
const UNOPENABLE: &str = "could not hand over";

/// How long a throttled count can wait to be written: the helper's report
/// interval plus its flusher's tick, with room to spare.
const THROTTLE_SETTLE: Duration = Duration::from_secs(8);

fn tail(log: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(log) else { return "<no log>".into() };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(12);
    lines[start..].join(" | ")
}

// ---------------------------------------------------------------------------
// the content source
// ---------------------------------------------------------------------------

/// `LocalDir` wrapped in a counting source whose fault injection can be
/// changed between scenarios. `LocalDir`'s own knobs are builder methods that
/// consume it, so a fresh one is built per fetch from the current settings and
/// the counting is done here.
struct TestSource {
    dir: PathBuf,
    fetches: AtomicU64,
    delay_ms: AtomicU64,
    /// Bytes after which the stream breaks, or `-1` for a source that works.
    fail_at: AtomicI64,
    /// Whether the break clears itself after one failed fetch.
    fail_once: AtomicBool,
}

impl TestSource {
    fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            dir,
            fetches: AtomicU64::new(0),
            delay_ms: AtomicU64::new(0),
            fail_at: AtomicI64::new(-1),
            fail_once: AtomicBool::new(false),
        })
    }

    fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.delay_ms.store(0, Ordering::SeqCst);
        self.fail_at.store(-1, Ordering::SeqCst);
        self.fail_once.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl ContentSource for TestSource {
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let mut local = LocalDir::new(self.dir.clone());
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            local = local.delay(Duration::from_millis(delay));
        }
        let fail_at = self.fail_at.load(Ordering::SeqCst);
        if fail_at >= 0 {
            local = local.fail_at(fail_at as u64);
            if self.fail_once.load(Ordering::SeqCst) {
                self.fail_at.store(-1, Ordering::SeqCst);
            }
        }
        local.fetch(item_id, from).await
    }
}

/// A second daemon connection from this uid that answers every hydration
/// request with one errno, for as long as it lives. This is how the errno
/// space is swept: `source::hydrate` clamps every value it produces, so the
/// only way to hand the helper an arbitrary one is to bypass the fill.
///
/// A connection of its own, and not an override in front of the real
/// daemon's request queue, because that override was a forwarding task with
/// a 64-deep channel of its own, and every hydration in the suite went
/// through it: the 3000-open burst measured the daemon with twice its real
/// buffering. As the newest connection of the uid, this one is
/// where the helper sends hydrations; dropped, it hands them
/// back to the daemon underneath.
struct Responder {
    errno: Arc<std::sync::atomic::AtomicI32>,
    control: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Responder {
    fn start() -> Result<Self, String> {
        let mut channel = raw_connect().map_err(|e| format!("cannot connect: {e}"))?;
        channel
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        match channel.recv::<ToDaemon>() {
            Ok((ToDaemon::Welcome { .. }, _)) => {}
            other => return Err(format!("not greeted: {other:?}")),
        }
        // The helper registers a connection before it reads anything from
        // it, so an answered `Hello` means hydrations now come here.
        channel
            .send(&ToHelper::Hello { version: PROTOCOL_VERSION }, None)
            .map_err(|e| e.to_string())?;
        match channel.recv::<ToDaemon>() {
            Ok((ToDaemon::Ack { errno: 0 }, _)) => {}
            other => return Err(format!("Hello not acknowledged: {other:?}")),
        }
        channel.get_ref().set_read_timeout(None).map_err(|e| e.to_string())?;
        let control = channel.get_ref().try_clone().map_err(|e| e.to_string())?;
        let errno = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let answer = Arc::clone(&errno);
        let thread = std::thread::spawn(move || loop {
            match channel.recv::<ToDaemon>() {
                Ok((ToDaemon::HydrateRequest { req_id }, fd)) => {
                    drop(fd);
                    let errno = answer.load(Ordering::SeqCst);
                    if channel.send(&ToHelper::HydrateDone { req_id, errno }, None).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        });
        Ok(Self { errno, control, thread: Some(thread) })
    }

    fn answer_with(&self, errno: i32) {
        self.errno.store(errno, Ordering::SeqCst);
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        let _ = self.control.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// readers, in other processes
// ---------------------------------------------------------------------------

struct Reader {
    pid: i32,
    started: Instant,
    /// What the reader got, and when it had finished.
    outcome: mpsc::Receiver<(Result<Vec<u8>, i32>, Instant)>,
}

impl Reader {
    fn start(exe: &Path, path: &Path) -> Result<Self, String> {
        let started = Instant::now();
        let child = Command::new(exe)
            .arg("--read")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start a reader: {e}"))?;
        let pid = child.id() as i32;
        let (tx, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            let result = match child.wait_with_output() {
                Ok(out) if out.status.success() => Ok(out.stdout),
                // A reader killed by a signal has no exit code; `-1` stands
                // for "did not return an errno at all".
                Ok(out) => Err(out.status.code().unwrap_or(-1)),
                Err(_) => Err(-1),
            };
            let _ = tx.send((result, Instant::now()));
        });
        Ok(Reader { pid, started, outcome })
    }

    fn kill(&self) {
        // SAFETY: a plain kill on a pid this process owns.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
    }

    fn get(&self, within: Duration) -> Result<Result<Vec<u8>, i32>, String> {
        self.get_timed(within).map(|(result, _)| result)
    }

    /// What the reader got, and how long after it was started it had it.
    fn get_timed(&self, within: Duration) -> Result<(Result<Vec<u8>, i32>, Duration), String> {
        self.outcome
            .recv_timeout(within)
            .map(|(result, at)| (result, at.duration_since(self.started)))
            .map_err(|e| format!("the reader never returned within {within:?}: {e}"))
    }

    /// [`get_timed`](Self::get_timed), or `None` if the reader is still
    /// waiting after `within` — in which case it can still be asked again.
    fn poll(&self, within: Duration) -> Option<(Result<Vec<u8>, i32>, Duration)> {
        self.outcome
            .recv_timeout(within)
            .ok()
            .map(|(result, at)| (result, at.duration_since(self.started)))
    }

    /// The content, or a description of why there is none.
    fn content(&self, within: Duration) -> Result<Vec<u8>, String> {
        match self.get(within)? {
            Ok(content) => Ok(content),
            Err(errno) => Err(format!("the open failed with errno {errno}")),
        }
    }

    /// The errno, or a complaint that the open succeeded.
    fn errno(&self, within: Duration) -> Result<i32, String> {
        match self.get(within)? {
            Ok(content) => Err(format!("the open succeeded, returning {} bytes", content.len())),
            Err(errno) => Ok(errno),
        }
    }
}

/// A second descriptor on a file, held by another process, which is what
/// `F_SETLEASE` refuses.
struct Holder {
    child: Child,
}

impl Holder {
    fn start(exe: &Path, path: &Path) -> Result<Self, String> {
        let mut child = Command::new(exe)
            .arg("--hold")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start a holder: {e}"))?;
        let mut line = String::new();
        let stdout = child.stdout.take().ok_or("the holder has no stdout")?;
        BufReader::new(stdout)
            .read_line(&mut line)
            .map_err(|e| format!("the holder said nothing: {e}"))?;
        if line.trim() != "held" {
            let _ = child.kill();
            return Err(format!("the holder could not open the file: {}", line.trim()));
        }
        Ok(Holder { child })
    }

    fn release(mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

struct Daemon {
    link: HelperLink,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct Ctx {
    fs: &'static str,
    exe: PathBuf,
    /// The registered sync root.
    root: PathBuf,
    /// Where `LocalDir` finds a payload for each item id.
    source_dir: PathBuf,
    /// A directory on the same filesystem, outside the root.
    outside: PathBuf,
    runtime: tokio::runtime::Runtime,
    helper: Mutex<HelperProc>,
    daemon: Mutex<Option<Daemon>>,
    sync_root: Mutex<SyncRoot>,
    source: Arc<TestSource>,
    locks: InodeLocks,
    /// Directories already announced to the helper, so a scenario that places
    /// three thousand placeholders in one directory does not send three
    /// thousand `MarkDir` requests. Cleared whenever the helper restarts,
    /// because its marks do not survive that on their own — the startup walk
    /// puts them back, but only for directories inside a registered root.
    marked: Mutex<std::collections::HashSet<PathBuf>>,
}

impl Ctx {
    fn helper_pid(&self) -> u32 {
        self.helper.lock().unwrap().pid()
    }

    fn link(&self) -> Result<HelperLink, String> {
        self.daemon
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.link.clone())
            .ok_or_else(|| "the daemon is not connected".to_string())
    }

    fn sync_root(&self) -> SyncRoot {
        self.sync_root.lock().unwrap().clone()
    }

    fn fetches(&self) -> u64 {
        self.source.fetches()
    }

    fn set_source_delay(&self, delay: Duration) {
        self.source.delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// Waits until the source has been asked for something it had not been
    /// asked for before.
    ///
    /// Sleeping a fixed time instead is what made "daemon death denies with
    /// EIO" measure the wrong thing: a reader that had not reached its
    /// `open()` before the daemon died was not a stranded waiter at all — it
    /// was a new open arriving with no daemon connected, which waits the full
    /// `DAEMON_WAIT` of 30 s and is then denied, which looks identical to a
    /// hang. A fetch having started is proof that the helper holds a job and
    /// that the daemon has been asked to fill it.
    fn wait_for_fetch(&self, before: u64, within: Duration) -> Result<(), String> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.fetches() > before {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(format!("no hydration had begun within {within:?}"))
    }

    fn daemon_connected(&self) -> bool {
        self.daemon.lock().unwrap().is_some()
    }

    /// A placeholder at `rel` inside the root, and the payload that fills it.
    /// Intermediate directories are created and marked, exactly as the daemon
    /// does while populating (invariant M1: marked before anything is created
    /// inside them).
    fn place(&self, rel: &str, item_id: &str, content: &[u8]) -> Result<PathBuf, String> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent)?;
        }
        std::fs::write(self.source_dir.join(item_id), content)
            .map_err(|e| format!("cannot write the payload: {e}"))?;
        let dir = File::open(path.parent().unwrap())
            .map_err(|e| format!("cannot open the parent directory: {e}"))?;
        let _ = std::fs::remove_file(&path);
        create_placeholder(
            &dir,
            path.file_name().unwrap().to_str().unwrap(),
            item_id,
            content.len() as u64,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .map_err(|e| format!("cannot create the placeholder: {e}"))?;
        Ok(path)
    }

    /// Creates a directory if it is not there and tells the helper about it,
    /// which is what the daemon does for every directory it creates.
    fn ensure_dir(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            std::fs::create_dir_all(path).map_err(|e| format!("cannot create {path:?}: {e}"))?;
        }
        if path.starts_with(&self.root) && path != self.root {
            if self.marked.lock().unwrap().contains(path) {
                return Ok(());
            }
            let dir = File::open(path).map_err(|e| format!("cannot open {path:?}: {e}"))?;
            let link = self.link()?;
            self.runtime
                .block_on(link.mark_dir(&dir))
                .map_err(|e| format!("cannot mark {path:?}: {e}"))?;
            self.marked.lock().unwrap().insert(path.to_path_buf());
        }
        Ok(())
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        let reader = Reader::start(&self.exe, path)?;
        reader.content(Duration::from_secs(60))
    }

    fn open_errno(&self, path: &Path) -> Result<i32, String> {
        let reader = Reader::start(&self.exe, path)?;
        reader.errno(Duration::from_secs(60))
    }

    fn state_of(&self, path: &Path) -> Result<Option<State>, String> {
        let file = File::open(path).map_err(|e| format!("cannot open {path:?}: {e}"))?;
        read_state(&file).map_err(|e| format!("cannot read the state of {path:?}: {e}"))
    }

    fn blocks_of(&self, path: &Path) -> Result<u64, String> {
        std::fs::metadata(path).map(|m| m.blocks()).map_err(|e| e.to_string())
    }

    fn ino_of(&self, path: &Path) -> Result<u64, String> {
        std::fs::metadata(path).map(|m| m.ino()).map_err(|e| e.to_string())
    }

    /// Asserts that a file holds no *data*, which is not the same as
    /// `st_blocks == 0`.
    ///
    /// Measured: on ext4 a fully punched placeholder still reports **8 blocks**
    /// (4 KiB), because its three `user.konedrive.*` xattrs do not fit in the
    /// inode and ext4 allocates a separate block for them — and `st_blocks`
    /// counts it. Btrfs and XFS report 0 for the same file, because they keep
    /// xattrs where `st_blocks` does not see them. A check that asserts zero
    /// therefore fails on ext4 for a file that is in exactly the right state.
    ///
    /// One filesystem block of slack is the discriminator: a file that kept
    /// its content shows its whole size in `st_blocks`, which for every
    /// placeholder this suite uses is far more than that.
    fn holds_no_data(&self, path: &Path) -> Result<(), String> {
        const METADATA_SLACK: u64 = 4096;
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        let allocated = meta.blocks() * 512;
        if allocated > METADATA_SLACK {
            return Err(format!(
                "{allocated} bytes are still allocated for a {}-byte file (more than the \
                 {METADATA_SLACK} bytes an xattr block can account for)",
                meta.len()
            ));
        }
        Ok(())
    }

    /// Kills the daemon side of the connection: every task that holds a
    /// `HelperLink` clone is aborted and the link dropped, which shuts the
    /// socket down and makes the helper run its disconnect cleanup.
    fn kill_daemon(&self) {
        if let Some(daemon) = self.daemon.lock().unwrap().take() {
            for task in &daemon.tasks {
                task.abort();
            }
            drop(daemon);
        }
        // Give the helper a moment to notice and drain the connection's jobs.
        std::thread::sleep(Duration::from_millis(200));
    }

    fn connect_daemon(&self) -> Result<(), String> {
        // The socket file exists from `bind`, which is a moment before
        // `listen`, so the first connect can legitimately be refused. Retrying
        // here is what replaced probing with a throwaway connection — see
        // `HelperProc::await_socket`.
        let deadline = Instant::now() + Duration::from_secs(20);
        let (link, requests) = loop {
            match self.runtime.block_on(HelperLink::connect(Path::new(SOCKET_PATH))) {
                Ok(pair) => break pair,
                Err(e) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                    let _ = e;
                }
                Err(e) => return Err(format!("cannot connect to the helper: {e}")),
            }
        };
        // The link's own request queue, straight into the loop production
        // runs: nothing in between may buffer.
        let serving = self.runtime.spawn(serve_hydrations(
            link.clone(),
            requests,
            Arc::clone(&self.source) as Arc<dyn ContentSource>,
            self.locks.clone(),
        ));
        *self.daemon.lock().unwrap() = Some(Daemon { link, tasks: vec![serving] });
        Ok(())
    }

    fn restart_helper(&self) -> Result<(), String> {
        self.kill_daemon();
        self.marked.lock().unwrap().clear();
        self.helper.lock().unwrap().restart()?;
        self.connect_daemon()
    }

    /// Restarts the helper with a descriptor limit, or a fault armed, or
    /// neither.
    fn restart_helper_with(
        &self,
        nofile: Option<u64>,
        fault: Option<(&str, &str)>,
    ) -> Result<(), String> {
        {
            let mut helper = self.helper.lock().unwrap();
            helper.nofile = nofile;
            helper.fault = fault.map(|(k, v)| (k.to_owned(), v.to_owned()));
        }
        self.restart_helper()
    }

    fn helper_alive(&self) -> bool {
        self.helper.lock().unwrap().alive()
    }

    /// Whether an armed fault really went off. Without it, a helper built
    /// without the `fault-injection` feature would make both
    /// unwind scenarios fail with a message about the unwind path — the one
    /// thing they would not have exercised at all.
    fn fault_fired(&self, name: &str) -> Result<(), String> {
        let log = self.helper.lock().unwrap().log.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if count_in_log(&log, &format!("{name}: deliberate panic")) > 0 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!(
            "{name} was armed but never fired: the helper under test was built without the \
             `fault-injection` feature (tests/vm/run.sh builds it with it), so this scenario \
             exercised nothing"
        ))
    }

    fn helper_log_tail(&self) -> String {
        let helper = self.helper.lock().unwrap();
        tail(&helper.log)
    }

    /// Waits for the helper to say something, and reports how long it took.
    /// The helper is another process with no interface but its journal, so
    /// this is the only way to tell "it has not noticed yet" from "it noticed
    /// and did the wrong thing".
    #[allow(dead_code)]
    fn wait_for_log(&self, needle: &str, within: Duration) -> Option<Duration> {
        let log = self.helper.lock().unwrap().log.clone();
        let before = count_in_log(&log, needle);
        let started = Instant::now();
        while started.elapsed() < within {
            if count_in_log(&log, needle) > before {
                return Some(started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// suite setup
// ---------------------------------------------------------------------------

fn statfs_type(path: &Path) -> Result<i64, String> {
    // SAFETY: `buf` is a live, correctly sized `statfs` that `statfs` fills.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(buf.f_type as i64)
}

fn run_suite(helper_binary: &Path, fs: &'static str, magic: i64) -> Result<Checks, String> {
    let mount = PathBuf::from(format!("/mnt/{fs}"));
    let found = statfs_type(&mount)?;
    if found != magic {
        return Err(format!(
            "/mnt/{fs} reports f_type {found:#x}, not {magic:#x}: the checks would measure the \
             wrong filesystem"
        ));
    }

    let base = mount.join("suite");
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let source_dir = base.join("source");
    let outside = base.join("outside");
    for dir in [&root, &source_dir, &outside] {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    }

    let mut facts = Checks::default();
    kernel_facts(fs, &mut facts);

    // A helper per filesystem, with no registrations carried over from the
    // previous one.
    let _ = std::fs::remove_file(ROOTS_FILE);
    let _ = std::fs::remove_file(SOCKET_PATH);
    let log = PathBuf::from(format!("/run/konedrive-helper-{fs}.log"));
    let _ = std::fs::remove_file(&log);
    let helper = HelperProc::start(helper_binary, &log)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let ctx = Ctx {
        fs,
        exe,
        root: root.clone(),
        source_dir: source_dir.clone(),
        outside,
        runtime,
        helper: Mutex::new(helper),
        daemon: Mutex::new(None),
        sync_root: Mutex::new(SyncRoot { path: root.clone(), root_id: String::new() }),
        source: TestSource::new(source_dir),
        locks: InodeLocks::new(),
        marked: Mutex::new(std::collections::HashSet::new()),
    };
    // Whatever happens below, the helper must not be left listening: the next
    // filesystem's run would find the socket taken, and a second helper with
    // its own marks on the same tree is not a state anything here reasons
    // about.
    let outcome = run_scenarios(&ctx, &root);
    ctx.kill_daemon();
    let log = ctx.helper.lock().unwrap().log.clone();
    ctx.helper.lock().unwrap().stop();
    let mut checks = match outcome {
        Ok(checks) => checks,
        Err(why) => {
            print_helper_log(&log);
            return Err(why);
        }
    };
    if !checks.failed.is_empty() {
        print_helper_log(&log);
    }
    checks.passed += facts.passed;
    checks.failed.extend(facts.failed);
    checks.timings.extend(facts.timings);
    Ok(checks)
}

/// The helper's own journal, printed when something failed. It lives on the
/// guest's tmpfs and goes away with the VM, so a failure nobody printed is a
/// failure nobody can diagnose.
fn print_helper_log(log: &Path) {
    let Ok(text) = std::fs::read_to_string(log) else { return };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(60);
    println!("  ---- the helper's last {} log line(s) ----", lines.len() - start);
    for line in &lines[start..] {
        println!("  | {line}");
    }
}

fn run_scenarios(ctx: &Ctx, root: &Path) -> Result<Checks, String> {
    ctx.connect_daemon()?;
    // Scoped to the registration, and that is load-bearing. A `HelperLink`
    // shuts its socket down only when the last clone goes, and this one used
    // to live as long as this function — the whole run. `kill_daemon`
    // therefore never ended the first connection: "daemon death denies with
    // EIO" waited for a disconnect that could not happen and failed on every
    // filesystem, and the connection it could not kill lived on beside every
    // later one, holding its stranded opener.
    let registered = {
        let link = ctx.link()?;
        ctx.runtime
            .block_on(root::register_root(&link, root))
            .map_err(|e| format!("cannot register {root:?}: {e}"))?
    };
    *ctx.sync_root.lock().unwrap() = registered;

    let mut checks = Checks::default();
    for (name, scenario) in scenarios() {
        if !wanted_scenario(name) {
            continue;
        }
        ctx.source.reset();
        now_running(&format!("[{}] {name}", ctx.fs));
        let started = Instant::now();
        let outcome = scenario(ctx, &mut checks);
        let elapsed = started.elapsed();
        checks.record_timed(ctx.fs, name, outcome, Some(elapsed));
        if elapsed > Duration::from_secs(30) {
            checks.note(ctx.fs, name, &format!("took {elapsed:?}"));
        }
        if !ctx.helper_alive() {
            checks.record(
                ctx.fs,
                "the helper is still running after that scenario",
                Err(format!("it exited; log: {}", ctx.helper_log_tail())),
            );
            // Nothing after this means anything without a helper.
            ctx.restart_helper()?;
        } else if !ctx.daemon_connected() {
            // A scenario that failed part-way through a deliberate disconnect
            // must not take the rest of the suite with it: without a daemon
            // the harness's own opens stop being exempt and every one of them
            // waits `DAEMON_WAIT` and is then denied.
            checks.note(ctx.fs, name, "left the daemon disconnected; reconnecting");
            ctx.connect_daemon()?;
        }
    }
    Ok(checks)
}

type Scenario = fn(&Ctx, &mut Checks) -> Result<(), String>;

fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("open fills the placeholder", open_fills),
        (
            "the ignore mark is really there, and the second open raises no event",
            second_open_is_ignored,
        ),
        ("mmap after open sees real content", mmap_sees_content),
        ("cp and cp --reflink copy real content", copy_sees_content),
        ("100 concurrent opens cause one fetch", one_fetch_for_many_openers),
        ("an instant HydrateDone never outruns the waiter", instant_reply),
        ("killing a waiting reader leaves others fine", killed_reader),
        ("daemon death denies with EIO", daemon_death_denies),
        ("daemon death denies the openers still waiting for credit", daemon_death_denies_queued),
        (
            "a same-uid connection that comes and goes hands control back to the live daemon",
            transient_connection_hands_back,
        ),
        ("a peer slow to read its Acks keeps its connection", pipelined_acks),
        ("helper restart re-marks every directory", helper_restart_remarks),
        ("download failure leaves online-only", failure_rolls_back),
        (
            "a request that waited for a fill slot does not re-fill a file filled meanwhile",
            stale_request_after_direct_fill,
        ),
        ("dehydrate while open is refused", dehydrate_in_use),
        ("dehydrate then open re-fetches, and clears the ignore mark", dehydrate_then_open),
        (
            "an ignore mark placed after a dehydration's ClearIgnore is taken off again",
            late_ignore_mark,
        ),
        (
            "a lease on one file does not stall the opens of others",
            leased_file_does_not_stall_others,
        ),
        (
            "an open through a read-only mount is refused by the kernel, and the helper keeps \
             running",
            readonly_mount_open_survived,
        ),
        (
            "a second open of a running executable is refused by the kernel, and the helper \
             keeps running",
            running_executable_open_survived,
        ),
        (
            "a file renamed during the registration walk was never emptied under its ignore mark",
            rename_during_registration_walk,
        ),
        (
            "every punch without interception clears the ignore mark first, or is refused while \
             a helper runs",
            punch_without_interception_rule,
        ),
        ("ClearIgnore is decided by who owns the file, on any filesystem", clear_ignore_by_ownership),
        (
            "recovery does not punch a file a fill from the previous connection finished \
             meanwhile",
            recovery_overtaken_by_old_fill,
        ),
        ("ClearIgnore after drop_caches keeps the connection", clear_ignore_after_reclaim),
        (
            "an idle file's ignore mark does not outlive UnregisterRoot into a later dehydration",
            unregistered_ignore_mark,
        ),
        (
            "a fill that finishes after a Forget leaves no mark once interception resumes",
            inflight_across_forget,
        ),
        (
            "a file carried back in with its ignore mark is cleared when the folder is registered",
            carried_in_ignore_mark,
        ),
        (
            "Forget with no helper link is refused for an intercepted folder, and nothing reads \
             zeros",
            forget_without_link_refused,
        ),
        (
            "an intercepted folder waiting for its helper at startup is not re-registered \
             without interception",
            pending_root_not_downgraded,
        ),
        ("Forget of a no-interception folder never asks the helper", no_interception_forget_is_local),
        (
            "populating a no-interception folder with a helper connected marks nothing",
            no_interception_populate_marks_nothing,
        ),
        (
            "a no-interception folder is populated and freed up with a helper connected",
            no_interception_with_helper_connected,
        ),
        (
            "a folder registered without the helper switches to interception when the helper starts",
            upgraded_when_the_helper_starts,
        ),
        ("directory created later is covered", new_directory_covered),
        ("file moved out keeps its individual mark", moved_out_still_covered),
        ("a hardlink in an unmarked directory, and a second mount", hardlink_and_second_mount),
        ("zero-byte file needs no fetch", zero_byte_file),
        ("the whole errno space is answered, and nobody is left hanging", errno_sweep),
        ("a hostile uid cannot touch another user's hydrations", hostile_uid),
        ("one uid cannot hold the helper's connections without bound", connections_per_uid_are_capped),
        ("SO_PEERCRED's pid is the pid the event reports", peercred_pid_matches_event_pid),
        (
            "the event fd is writable through a read-only mount of the same tree",
            readonly_mount_event_fd,
        ),
        (
            "the startup walk against a symlinked subdirectory and a tree changing under it",
            startup_walk_hazards,
        ),
        ("a filesystem reporting DT_UNKNOWN is still walked", dt_unknown_walk),
        ("a real EMFILE does not end the helper", emfile_survived),
        ("a panic in a worker is contained", worker_panic_contained),
        (
            "a panic on a connection denies its openers and keeps the helper",
            connection_panic_contained,
        ),
        ("disk full denies with ENOSPC or EIO and never commits", disk_full),
        ("a subtree on another filesystem is counted, not silently skipped", cross_device_recovery),
        ("the waiter caps bound how many opens may wait for a daemon", waiter_caps),
        ("a burst of several thousand concurrent opens loses nobody", burst),
        ("killing the helper mid-flight allows every suspended open", helper_death_allows),
    ]
}

// ---------------------------------------------------------------------------
// scenarios
// ---------------------------------------------------------------------------

fn open_fills(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("report.pdf", "ITEM1", b"REAL CONTENT")?;
    let before = ctx.fetches();
    let content = ctx.read(&path)?;
    if content != b"REAL CONTENT" {
        return Err(format!("the reader got {content:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected exactly one fetch, saw {}", ctx.fetches() - before));
    }
    match ctx.state_of(&path)? {
        Some(State::Hydrated) => Ok(()),
        other => Err(format!("the state is {other:?}")),
    }
}

/// The two halves of review item 1, which have to be asked together: the mark
/// really exists (fdinfo, never the syscall's answer), and the next open does
/// not reach the helper.
///
/// "Did not reach the helper" needs a discriminator, because a second open
/// that *did* reach it would be allowed anyway — the file is `hydrated`. So
/// the file's state xattr is made unreadable first: an open the helper sees is
/// denied `EIO` (§5.2 never allows what it cannot vouch for), and an open it
/// does not see succeeds. The control below proves the discriminator bites.
fn second_open_is_ignored(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("twice.bin", "ITEM2", b"TWICE")?;
    let content = ctx.read(&path)?;
    if content != b"TWICE" {
        return Err(format!("the first read returned {content:?}"));
    }
    let ino = ctx.ino_of(&path)?;
    if !ignore_mark_present(ctx.helper_pid(), ino) {
        return Err(format!(
            "no ignore mark for ino {ino} in /proc/{}/fdinfo after a completed hydration",
            ctx.helper_pid()
        ));
    }

    // The control: a managed file with a corrupt state and no ignore mark is
    // denied. Without this, "the open succeeded" below would not be evidence.
    let control = ctx.place("control.bin", "ITEM2C", b"CONTROL")?;
    corrupt_state(&control)?;
    let errno = ctx.open_errno(&control)?;
    if errno != libc::EIO {
        return Err(format!(
            "the control is not discriminating: an intercepted open of a file with a corrupt \
             state gave errno {errno}, not EIO"
        ));
    }

    restore_state(&control, State::Hydrated)?;

    corrupt_state(&path)?;
    if !ignore_mark_present(ctx.helper_pid(), ino) {
        return Err("the ignore mark did not survive the file being modified".into());
    }
    let again = ctx.read(&path).map_err(|e| {
        format!("the second open reached the helper despite the ignore mark: {e}")
    })?;
    if again != b"TWICE" {
        return Err(format!("the second read returned {again:?}"));
    }
    checks.note(
        ctx.fs,
        "ignore mark",
        &format!("mflags for ino {ino}: {:?}", mflags_of(ctx.helper_pid(), ino)),
    );
    restore_state(&path, State::Hydrated)?;
    Ok(())
}

fn mflags_of(pid: u32, ino: u64) -> Option<String> {
    helper_marks(pid)
        .iter()
        .find(|m| m.ino == ino && m.ignored_mask & FAN_OPEN_PERM != 0)
        .map(|m| format!("{:x}", m.mflags))
}

fn corrupt_state(path: &Path) -> Result<(), String> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("cannot open {path:?}: {e}"))?;
    file.set_xattr(XATTR_STATE, b"not-a-state").map_err(|e| e.to_string())
}

fn restore_state(path: &Path, state: State) -> Result<(), String> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("cannot open {path:?}: {e}"))?;
    write_state(&file, state).map_err(|e| e.to_string())
}

fn mmap_sees_content(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let payload: Vec<u8> = (0..(1usize << 20)).map(|i| (i % 251) as u8).collect();
    std::fs::write(ctx.source_dir.join("ITEM_MMAP"), &payload).map_err(|e| e.to_string())?;
    let path = ctx.place("mapped.bin", "ITEM_MMAP", &payload)?;
    let before = ctx.fetches();
    // The open that hydrates has to be intercepted, so it runs elsewhere; the
    // mapping is then of a file that is already full.
    let content = ctx.read(&path)?;
    if content != payload {
        return Err("the reader got the wrong content".into());
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    let file = File::open(&path).map_err(|e| e.to_string())?;
    let len = payload.len();
    // SAFETY: a private read-only mapping of `len` bytes of an open file of
    // exactly that size; unmapped below before `file` is dropped.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            std::os::fd::AsRawFd::as_raw_fd(&file),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the mapping above is live and `len` bytes long.
    let mapped = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let middle = len / 2;
    let ok = mapped[middle..middle + 4096] == payload[middle..middle + 4096];
    // SAFETY: unmapping exactly what was mapped.
    unsafe { libc::munmap(addr, len) };
    if !ok {
        return Err("the middle page of the mapping is not the payload".into());
    }
    Ok(())
}

fn copy_sees_content(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let payload: Vec<u8> = (0..(64usize * 1024)).map(|i| (i % 253) as u8).collect();
    let path = ctx.place("copied.bin", "ITEM_CP", &payload)?;
    let plain = ctx.outside.join("plain-copy.bin");
    let _ = std::fs::remove_file(&plain);
    // `cp` is another process, so its open is the intercepted one.
    let status = Command::new("cp")
        .arg(&path)
        .arg(&plain)
        .status()
        .map_err(|e| format!("cannot run cp: {e}"))?;
    if !status.success() {
        return Err(format!("cp failed: {status}"));
    }
    let copied = std::fs::read(&plain).map_err(|e| e.to_string())?;
    if copied != payload {
        return Err("the plain copy is not the payload".into());
    }

    if ctx.fs == "btrfs" {
        let reflink = ctx.outside.join("reflink-copy.bin");
        let _ = std::fs::remove_file(&reflink);
        let output = Command::new("cp")
            .arg("--reflink=always")
            .arg(&path)
            .arg(&reflink)
            .output()
            .map_err(|e| format!("cannot run cp --reflink: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "cp --reflink=always failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let copied = std::fs::read(&reflink).map_err(|e| e.to_string())?;
        if copied != payload {
            return Err("the reflink copy is not the payload".into());
        }
    } else {
        checks.note(ctx.fs, "cp --reflink", "not attempted: this filesystem has no reflinks");
    }
    Ok(())
}

fn one_fetch_for_many_openers(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let payload = vec![9u8; 1 << 20];
    let path = ctx.place("popular.bin", "ITEM3", &payload)?;
    ctx.set_source_delay(Duration::from_millis(500));
    let before = ctx.fetches();
    let readers: Vec<Reader> = (0..100)
        .map(|_| Reader::start(&ctx.exe, &path))
        .collect::<Result<_, _>>()?;
    for reader in &readers {
        let content = reader.content(Duration::from_secs(60))?;
        if content.len() != payload.len() {
            return Err(format!("a reader got {} bytes", content.len()));
        }
        if content != payload {
            return Err("a reader got the wrong content".into());
        }
    }
    let fetches = ctx.fetches() - before;
    if fetches != 1 {
        return Err(format!("expected one fetch for 100 openers, saw {fetches}"));
    }
    Ok(())
}

/// Review item 3. The helper enrols the waiter in the same step that claims
/// the job (`jobs::enroll`), so a `HydrateDone` cannot arrive before there is
/// anybody to answer. Driven with a daemon that replies as fast as it can, one
/// file at a time, so that the send and the reply race as tightly as the
/// machine allows.
fn instant_reply(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    for i in 0..200 {
        let name = format!("instant-{i}.bin");
        let item = format!("ITEM_INSTANT_{i}");
        let path = ctx.place(&name, &item, b"I")?;
        let reader = Reader::start(&ctx.exe, &path)?;
        let content = reader
            .content(Duration::from_secs(20))
            .map_err(|e| format!("opener {i} was left unanswered: {e}"))?;
        if content != b"I" {
            return Err(format!("opener {i} got {content:?}"));
        }
    }
    Ok(())
}

fn killed_reader(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("shared.bin", "ITEM_KILL", b"SURVIVOR")?;
    ctx.set_source_delay(Duration::from_secs(2));
    let before = ctx.fetches();
    let doomed = Reader::start(&ctx.exe, &path)?;
    let survivor = Reader::start(&ctx.exe, &path)?;
    std::thread::sleep(Duration::from_millis(400));
    doomed.kill();
    let content = survivor.content(Duration::from_secs(30))?;
    if content != b"SURVIVOR" {
        return Err(format!("the surviving reader got {content:?}"));
    }
    let fetches = ctx.fetches() - before;
    if fetches != 1 {
        return Err(format!("expected one fetch, saw {fetches}"));
    }
    if !ctx.helper_alive() {
        return Err("the helper exited".into());
    }
    Ok(())
}

fn daemon_death_denies(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("orphan.bin", "ITEM_ORPHAN", b"NEVER ARRIVES")?;
    ctx.set_source_delay(Duration::from_secs(20));
    let before = ctx.fetches();
    let reader = Reader::start(&ctx.exe, &path)?;
    // The reader has to be a *stranded waiter* before the daemon dies, not an
    // open that merely has not happened yet: those two are indistinguishable
    // from the outside and the second one legitimately takes `DAEMON_WAIT`.
    let outcome = (|| -> Result<(), String> {
        ctx.wait_for_fetch(before, Duration::from_secs(20))?;
        // `kill_daemon` drops every `HelperLink` handle, which is what a
        // daemon process dying does to its socket. Whether the
        // helper *notices* is the thing under test, and it has no interface
        // but its journal.
        let log = ctx.helper.lock().unwrap().log.clone();
        let lines_before = std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
        // Counted here, before the kill, not inside the watcher: the watcher
        // thread can start after the helper has already noticed — once the
        // disconnect really happens, it takes about a millisecond — and a
        // baseline taken then already contains the line it is waiting for.
        let before = count_in_log(&log, "went away with");
        let watching = std::thread::spawn({
            let log = log.clone();
            move || {
                let started = Instant::now();
                while started.elapsed() < Duration::from_secs(20) {
                    if count_in_log(&log, "went away with") > before {
                        return Some(started.elapsed());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                None
            }
        });
        ctx.kill_daemon();
        let noticed = watching.join().unwrap_or(None);
        let Some(noticed) = noticed else {
            // What the helper said about its connections meanwhile is the
            // only way to tell "the socket never closed" from "it closed,
            // but the opener's job was not on it".
            let said: Vec<String> = std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .skip(lines_before)
                .filter(|l| l.contains("connection") || l.contains("daemon"))
                .map(str::to_owned)
                .collect();
            return Err(format!(
                "the helper never noticed the daemon was gone: dropping every `HelperLink` \
                 handle did not tear the connection down, so the suspended opener was never \
                 denied; the helper said: {said:?}"
            ));
        };
        let errno = reader.errno(Duration::from_secs(30)).map_err(|e| {
            format!("{e} (the helper noticed the disconnect after {noticed:?})")
        })?;
        if errno != libc::EIO {
            return Err(format!("the reader got errno {errno}, not EIO"));
        }
        if !ctx.helper_alive() {
            return Err("the helper exited when the daemon did".into());
        }
        Ok(())
    })();
    reader.kill();
    ctx.source.reset();
    // Whatever happened, a suite without a daemon is a suite in which the
    // harness's own opens are no longer exempt and block for 30 s each.
    if !ctx.daemon_connected() {
        ctx.connect_daemon()?;
    }
    outcome
}

/// The disconnect half. A hydration waiting for credit belongs to
/// its connection as much as one already sent, and goes with it. With a slow
/// source, twice the credit's worth of openers are suspended — at most
/// `MAX_OUTSTANDING_HYDRATIONS` of them sent to the daemon, the rest enrolled
/// in the helper and not yet sent — and then the daemon dies. Every one must
/// be denied `EIO`: none left suspended, none let through onto an unfilled
/// file, and none refused `EAGAIN` up front, which is what the 65th opener got
/// before the queue existed.
fn daemon_death_denies_queued(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("queued");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 2 * konedrive_proto::MAX_OUTSTANDING_HYDRATIONS + 8;
    let payload = vec![0x66u8; 4096];
    for i in 0..count {
        ctx.place(&format!("queued/burst-{i}"), &format!("ITEM_QUEUED_{i}"), &payload)?;
    }
    ctx.set_source_delay(Duration::from_secs(20));
    let log = ctx.helper.lock().unwrap().log.clone();
    let lines_before = std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
    let before = ctx.fetches();

    let report = std::thread::scope(|scope| {
        scope.spawn(|| {
            // Once the first fill has begun the burst's openers are all
            // arriving; a second and a half more is time enough for every one
            // of them to be enrolled, and nowhere near the twenty seconds the
            // first fill takes.
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && ctx.fetches() <= before {
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(1500));
            ctx.kill_daemon();
        });
        run_burst(ctx, &dir, count, 0x66)
    });
    ctx.source.reset();
    let outcome = (|| -> Result<(), String> {
        let report = report?;
        // What the helper held for the connection when it went: the jobs its
        // disconnect guard denied. Every opener should have been one of them.
        let stranded: Vec<String> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .skip(lines_before)
            .filter(|l| l.contains("went away with"))
            .map(str::to_owned)
            .collect();
        checks.note(
            ctx.fs,
            "daemon death, queued",
            &format!("{count} openers, daemon killed: {report}; the helper said: {stranded:?}"),
        );
        if report.answered != count {
            return Err(format!("{} opener(s) were never answered", count - report.answered));
        }
        if report.ok != 0 || report.wrong != 0 {
            return Err(format!(
                "{} opener(s) got through with no daemon left to fill them ({} of them onto an \
                 unfilled file)",
                report.ok + report.wrong,
                report.wrong
            ));
        }
        let eio = report.errors.get(&libc::EIO).copied().unwrap_or(0);
        if eio != count {
            return Err(format!(
                "every opener should have been denied EIO with its connection, but they got \
                 {:?} (errno -> openers)",
                report.errors
            ));
        }
        let held = stranded.iter().find_map(|line| {
            let after = line.split("went away with ").nth(1)?;
            after.split_whitespace().next()?.parse::<usize>().ok()
        });
        if held != Some(count) {
            return Err(format!(
                "the connection held {held:?} hydrations when it went, not all {count}: the \
                 openers were not all enrolled before the daemon died, so this run did not \
                 measure the queued ones"
            ));
        }
        Ok(())
    })();
    if !ctx.daemon_connected() {
        ctx.connect_daemon()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A process of the daemon's own uid — a CLI, a second instance, a
/// probe — connects after it and then goes away. The helper routes a uid's
/// hydrations to its newest connection, so while the newer one is there it is
/// where they go; when it leaves, the live daemon underneath must get them
/// back.
///
/// Before the per-uid stack it did not: the newer connection replaced the
/// uid's one registration, its own cleanup then removed it, and the live
/// daemon — whose socket was still open, so it had no reason to reconnect —
/// was left unregistered. Every later open waited `DAEMON_WAIT` and was denied
/// `EIO`, until the daemon restarted. No scheduling luck was needed.
fn transient_connection_hands_back(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let routed = ctx.place("transient/routed.bin", "ITEM_TRANSIENT_ROUTED", b"ROUTED")?;
    let after =
        ctx.place("transient/after.bin", "ITEM_TRANSIENT_AFTER", b"THE LIVE DAEMON AGAIN")?;
    let outcome = (|| -> Result<(), String> {
        let mut transient =
            raw_connect().map_err(|e| format!("cannot open a second connection: {e}"))?;
        transient
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        match transient.recv::<ToDaemon>() {
            Ok((ToDaemon::Welcome { .. }, _)) => {}
            other => return Err(format!("the second connection was not greeted: {other:?}")),
        }
        // The newer connection must really have become the one hydrations go
        // to, or its leaving would hand nothing back and this would pass for
        // the wrong reason.
        let before = ctx.fetches();
        let stranded = Reader::start(&ctx.exe, &routed)?;
        match transient.recv::<ToDaemon>() {
            Ok((ToDaemon::HydrateRequest { .. }, Some(_))) => {}
            other => {
                return Err(format!(
                    "the newer connection was never asked to hydrate (it got {other:?}, and the \
                     live daemon fetched {} time(s)): it never became the one hydrations go to",
                    ctx.fetches() - before
                ))
            }
        }
        drop(transient);
        // Its own opener goes with it: a connection's hydrations are denied
        // when it disconnects, whoever sits underneath.
        let errno = stranded.errno(Duration::from_secs(10))?;
        if errno != libc::EIO {
            return Err(format!("the newer connection's own opener got errno {errno}, not EIO"));
        }

        let started = Instant::now();
        let got = Reader::start(&ctx.exe, &after)?.get(Duration::from_secs(60))?;
        let took = started.elapsed();
        checks.note(
            ctx.fs,
            "transient connection",
            &format!(
                "after the newer same-uid connection left, the next open took {took:?} and {}",
                match &got {
                    Ok(content) => format!("read {} bytes", content.len()),
                    Err(errno) => format!("was denied errno {errno}"),
                }
            ),
        );
        match got {
            Ok(content) if content == b"THE LIVE DAEMON AGAIN" => {}
            Ok(content) => return Err(format!("the open read {content:?}")),
            Err(errno) => {
                return Err(format!(
                    "after a newer same-uid connection came and went, an open was denied errno \
                     {errno} after {took:?}: the live daemon underneath no longer receives \
                     hydrations, and nothing will make it reconnect"
                ))
            }
        }
        if ctx.fetches() != before + 1 {
            return Err(format!(
                "the live daemon fetched {} time(s), not once",
                ctx.fetches() - before
            ));
        }
        Ok(())
    })();
    if outcome.is_err() {
        // Whatever the helper now believes about this uid, the rest of the
        // suite needs a daemon that hydrations actually reach.
        ctx.kill_daemon();
        ctx.connect_daemon()?;
    }
    let _ = std::fs::remove_dir_all(ctx.root.join("transient"));
    outcome
}

/// A peer that sends faster than it reads its replies — two
/// thousand requests before it reads a single `Ack` — is backpressure, not a
/// dead peer, and keeps its connection. It used to lose it: `serve_one` ended
/// any connection whose `Ack` did not fit in the outbox, which a peer reaches
/// by being slow to read for a few hundred requests, whatever it is.
///
/// The peer is a uid with no root, so that nothing it does touches the routing
/// of the suite's own daemon; `serve_one` is the same code for every
/// connection.
fn pipelined_acks(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    const COUNT: usize = 2000;
    // The suite's binary lives where another uid cannot traverse; the same
    // copy the hostile client uses.
    let client = Path::new("/run/konedrive-pipeline-client");
    std::fs::copy(&ctx.exe, client).map_err(|e| format!("cannot stage the client: {e}"))?;
    std::fs::set_permissions(client, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let output = Command::new(client)
        .arg("--pipeline")
        .arg(COUNT.to_string())
        .uid(HOSTILE_UID)
        .gid(HOSTILE_UID)
        .output()
        .map_err(|e| format!("cannot run the client: {e}"))?;
    let said = String::from_utf8_lossy(&output.stdout).into_owned();
    checks.note(ctx.fs, "pipelined Acks", &said.lines().collect::<Vec<_>>().join("; "));
    let acks = said
        .lines()
        .find_map(|l| l.strip_prefix("ACKS "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or_else(|| format!("the client reported no count: {said:?}"))?;
    if acks != COUNT {
        return Err(format!(
            "a peer that was slow to read got {acks} of its {COUNT} Acks and then lost its \
             connection: the helper ended it for backpressure ({said:?})"
        ));
    }
    if !said.lines().any(|l| l == "ALIVE") {
        return Err(format!("the connection did not survive the pipelining: {said:?}"));
    }
    if !ctx.helper_alive() {
        return Err("the helper did not survive a pipelining peer".into());
    }
    Ok(())
}

fn helper_restart_remarks(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("nested/deep/after-restart.bin", "ITEM_RESTART", b"RESTARTED")?;
    let dir_ino = ctx.ino_of(&ctx.root.join("nested/deep"))?;
    ctx.restart_helper()?;
    if !dir_mark_present(ctx.helper_pid(), dir_ino) {
        return Err(format!(
            "the nested directory (ino {dir_ino}) carries no mark after the startup walk"
        ));
    }
    let before = ctx.fetches();
    let content = ctx.read(&path)?;
    if content != b"RESTARTED" {
        return Err(format!("the reader got {content:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

fn failure_rolls_back(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let payload = vec![7u8; 4096];
    let path = ctx.place("broken.bin", "ITEM_FAIL", &payload)?;
    ctx.source.fail_at.store(1024, Ordering::SeqCst);
    ctx.source.fail_once.store(false, Ordering::SeqCst);
    let errno = ctx.open_errno(&path)?;
    if errno != libc::EIO {
        return Err(format!("the failed open gave errno {errno}, not EIO"));
    }
    match ctx.state_of(&path)? {
        Some(State::OnlineOnly) => {}
        other => return Err(format!("the state after a failed fill is {other:?}")),
    }
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() != payload.len() as u64 {
        return Err(format!("the size is {} after the roll-back", meta.len()));
    }
    ctx.holds_no_data(&path).map_err(|e| format!("after the roll-back, {e}"))?;

    ctx.source.reset();
    let content = ctx.read(&path)?;
    if content != payload {
        return Err("the later, successful open did not fill the file".into());
    }
    Ok(())
}

fn dehydrate_in_use(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let payload = vec![3u8; 8192];
    let path = ctx.place("busy.bin", "ITEM_BUSY", &payload)?;
    let content = ctx.read(&path)?;
    if content != payload {
        return Err("the file was not hydrated first".into());
    }
    let holder = Holder::start(&ctx.exe, &path)?;
    let link = ctx.link()?;
    let sync_root = ctx.sync_root();
    let outcome = ctx.runtime.block_on(root::dehydrate(&link, &sync_root, &path));
    let result = match outcome {
        Err(DehydrateError::InUse) => Ok(()),
        Err(other) => Err(format!("the dehydration was refused with {other}, not \"in use\"")),
        Ok(()) => Err("the dehydration went ahead while the file was open".into()),
    };
    holder.release();
    result?;
    let still = std::fs::read(&path).map_err(|e| e.to_string())?;
    if still != payload {
        return Err("the refused dehydration destroyed the content anyway".into());
    }
    match ctx.state_of(&path)? {
        Some(State::Hydrated) => Ok(()),
        other => Err(format!("a refused dehydration left the state at {other:?}")),
    }
}

/// The original proposal's scenario: after a dehydration the file must
/// carry **no** ignore mark — punched *and* still ignored, it would be empty
/// with every later open suppressed, reading zeros for as long as the inode
/// stays cached. The dehydration runs on the one descriptor it opened before
/// its first check, so it raises no open of its own and depends
/// on no exemption; what this checks is that its `ClearIgnore` really took
/// the mark off, on fdinfo, and that the next open re-fetches.
fn dehydrate_then_open(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let payload = vec![5u8; 16384];
    let path = ctx.place("cycle.bin", "ITEM_CYCLE", &payload)?;
    let content = ctx.read(&path)?;
    if content != payload {
        return Err("the file was not hydrated first".into());
    }
    let ino = ctx.ino_of(&path)?;
    if !ignore_mark_present(ctx.helper_pid(), ino) {
        return Err("no ignore mark after the hydration, so the dehydration proves nothing".into());
    }

    let link = ctx.link()?;
    let sync_root = ctx.sync_root();
    ctx.runtime
        .block_on(root::dehydrate(&link, &sync_root, &path))
        .map_err(|e| format!("the dehydration failed: {e}"))?;

    if ignore_mark_present(ctx.helper_pid(), ino) {
        return Err(format!(
            "ino {ino} still carries an ignore mark after being dehydrated: it is now empty AND \
             un-intercepted, and every later open reads zeros"
        ));
    }
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() != payload.len() as u64 {
        return Err(format!("dehydration changed the size to {}", meta.len()));
    }
    ctx.holds_no_data(&path).map_err(|e| format!("after the punch, {e}"))?;
    match ctx.state_of(&path)? {
        Some(State::OnlineOnly) => {}
        other => return Err(format!("the state after dehydration is {other:?}")),
    }

    let before = ctx.fetches();
    let again = ctx.read(&path)?;
    if again != payload {
        return Err("the open after dehydration did not re-fill the file".into());
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one more fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

/// Review item 4. An evictable mark is designed to vanish, so `ClearIgnore`
/// meets a mark that is not there as a matter of routine; if that ended the
/// connection, every hydration in flight would be denied along with it.
fn clear_ignore_after_reclaim(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("reclaimed.bin", "ITEM_RECLAIM", b"RECLAIMED")?;
    let content = ctx.read(&path)?;
    if content != b"RECLAIMED" {
        return Err("the file was not hydrated first".into());
    }
    let ino = ctx.ino_of(&path)?;
    if !ignore_mark_present(ctx.helper_pid(), ino) {
        return Err("no ignore mark to reclaim".into());
    }
    drop_caches()?;
    let still_there = ignore_mark_present(ctx.helper_pid(), ino);
    checks.note(
        ctx.fs,
        "drop_caches",
        if still_there {
            "the evictable ignore mark survived drop_caches (the inode was still pinned)"
        } else {
            "the evictable ignore mark is gone after drop_caches, as §8's memory argument needs"
        },
    );

    let file = File::options().read(true).write(true).open(&path).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    ctx.runtime
        .block_on(link.clear_ignore(&file))
        .map_err(|e| format!("ClearIgnore on a reclaimed mark was refused: {e}"))?;

    // The connection must still work afterwards: the failure this guards
    // against is the whole link going down, not one bad answer.
    let next = ctx.place("after-clear.bin", "ITEM_AFTERCLEAR", b"STILL HERE")?;
    let content = ctx.read(&next).map_err(|e| {
        format!("the daemon connection did not survive a ClearIgnore on a reclaimed mark: {e}")
    })?;
    if content != b"STILL HERE" {
        return Err(format!("the follow-up read returned {content:?}"));
    }
    Ok(())
}

/// Small round 3, item 1: does the ignore mark of a hydrated file that is
/// **idle** when its folder is forgotten outlive `UnregisterRoot`, and can a
/// later dehydration then leave it empty *and* un-intercepted?
///
/// Only the idle case, which `marks::walk_and_unmark` covers by clearing the
/// file's mark as it passes. A hydration still in flight at the Forget marks
/// its file *after* that walk has passed it — `inflight_across_forget` — and a
/// file carried out of the folder is never passed at all —
/// `carried_in_ignore_mark`; both are covered by the registration walk
///.
///
/// The sequence a reasoned out, driven through the daemon's own
/// `SyncService` so that every step is the code a real daemon runs:
///
/// 1. a folder registered **with** interception; a file in it hydrated by an
///    intercepted open, so the helper puts an ignore mark on it;
/// 2. the folder unregistered — the walk must have taken the idle file's
///    ignore mark off, measured on fdinfo — then registered again
///    **without** interception while the daemon has no helper link — the
///    helper is still running with the same fanotify group;
/// 3. the file freed up: with no link while the helper runs, that is
/// refused and nothing changes (local rule — it used to be
///    punched with no `ClearIgnore`, which is what made step 2 matter); with
///    the link back, the helper clears the mark and the file is punched;
/// 4. the folder registered **with** interception again, the inode still in
///    cache;
/// 5. the file opened from another process.
///
/// If an ignore mark were left on the emptied file, step 5 would raise no
/// event at all and the reader would get the file's size in zeros. Every step
/// is measured on `/proc/<helper>/fdinfo` and on what the reader gets; the
/// failure message carries the whole trace, so a red run says which step
/// went wrong.
fn unregistered_ignore_mark(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = ctx.root.parent().ok_or("the suite root has no parent")?.join("reregistered");
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir(&folder).map_err(|e| format!("cannot create {folder:?}: {e}"))?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let result = unregistered_ignore_mark_steps(ctx, checks, &service, &link, &folder);

    // Whatever happened, the helper must not keep this folder: a later
    // scenario would otherwise share the filesystem with a registration
    // nobody here reasons about.
    service.set_link(Some(link));
    if service.root().is_some() {
        let _ = ctx.runtime.block_on(service.unregister_root());
    }
    drop(service);
    let _ = std::fs::remove_dir_all(&folder);
    result
}

fn unregistered_ignore_mark_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    service: &SyncService,
    link: &HelperLink,
    folder: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let folder_ino = ctx.ino_of(folder)?;
    let mut trace: Vec<String> = Vec::new();

    // 1. Registered with interception, and a file hydrated through it.
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    if !dir_mark_present(pid, folder_ino) {
        return Err("the folder carries no directory mark after RegisterRoot".into());
    }
    let payload: Vec<u8> = (0..(64usize * 1024)).map(|i| (i % 241) as u8 + 1).collect();
    std::fs::write(ctx.source_dir.join("ITEM_REREG"), &payload)
        .map_err(|e| format!("cannot write the payload: {e}"))?;
    let dir = File::open(folder).map_err(|e| e.to_string())?;
    create_placeholder(
        &dir,
        "kept.bin",
        "ITEM_REREG",
        payload.len() as u64,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    )
    .map_err(|e| format!("cannot create the placeholder: {e}"))?;
    drop(dir);
    let path = folder.join("kept.bin");
    let ino = ctx.ino_of(&path)?;
    if ctx.read(&path)? != payload {
        return Err("the first open did not fill the file".into());
    }
    if !ignore_mark_present(pid, ino) {
        return Err("no ignore mark after the hydration, so nothing below would be tested".into());
    }
    trace.push("hydrated, ignore mark present".into());

    // 2. Unregistered — the helper is told — then registered without
    //    interception while the daemon has no link to it.
    ctx.runtime
        .block_on(service.unregister_root())
        .map_err(|e| format!("cannot unregister the folder: {e}"))?;
    let dir_after = dir_mark_present(pid, folder_ino);
    let ignore_after = ignore_mark_present(pid, ino);
    trace.push(format!(
        "after UnregisterRoot: directory mark {}, ignore mark {}",
        present(dir_after),
        present(ignore_after)
    ));
    if dir_after {
        return Err(format!("{}; UnregisterRoot did not unmark the folder", trace.join("; ")));
    }
    if ignore_after {
        return Err(format!(
            "{}; UnregisterRoot's walk left the idle file's ignore mark on",
            trace.join("; ")
        ));
    }
    service.set_link(None);
    ctx.runtime
        .block_on(service.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register the folder without interception: {e}"))?;

    // 3. With no link while the helper runs, refused; with it, cleared and
    //    punched.
    let refused = ctx.runtime.block_on(service.dehydrate(&path));
    trace.push(format!(
        "freed up with no link while the helper runs → {}, state {:?}",
        outcome(&refused),
        ctx.state_of(&path)?
    ));
    if !matches!(refused, Err(SyncError::NoHelper)) || ctx.holds_no_data(&path).is_ok() {
        return Err(format!(
            "{}; with a helper running and no link to it, nothing may be emptied",
            trace.join("; ")
        ));
    }
    service.set_link(Some(link.clone()));
    ctx.runtime
        .block_on(service.dehydrate(&path))
        .map_err(|e| format!("{}; the dehydration failed: {e}", trace.join("; ")))?;
    ctx.holds_no_data(&path).map_err(|e| format!("after the dehydration, {e}"))?;
    match ctx.state_of(&path)? {
        Some(State::OnlineOnly) => {}
        other => return Err(format!("the state after the dehydration is {other:?}")),
    }
    trace.push(format!(
        "freed up with the link: no data, online-only, ignore mark {}",
        present(ignore_mark_present(pid, ino))
    ));

    // 4. Registered with interception again, the inode still in cache.
    ctx.runtime
        .block_on(service.unregister_root())
        .map_err(|e| format!("cannot forget the unintercepted registration: {e}"))?;
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register the folder with interception again: {e}"))?;
    if !dir_mark_present(pid, folder_ino) {
        return Err("the folder carries no directory mark after the second RegisterRoot".into());
    }
    trace.push(format!(
        "registered with interception again: directory mark present, ignore mark {}",
        present(ignore_mark_present(pid, ino))
    ));

    // 5. What a reader in another process actually gets.
    let before = ctx.fetches();
    let content = ctx.read(&path)?;
    let fetched = ctx.fetches() - before;
    let zeros = content.iter().filter(|b| **b == 0).count();
    trace.push(format!(
        "the reader got {} bytes, {zeros} of them zero, after {fetched} fetch(es)",
        content.len()
    ));
    checks.note(ctx.fs, "unregistered ignore mark", &trace.join("; "));
    if content != payload || fetched != 1 {
        return Err(format!(
            "{}. The open was not intercepted: the file is empty AND ignored, and reads zeros \
             for as long as the inode stays in cache",
            trace.join("; ")
        ));
    }
    Ok(())
}

/// A fresh, empty folder beside the suite root, on the filesystem under test:
/// somewhere a scenario can register, fill and forget a root of its own
/// without touching the one every other scenario shares.
fn scenario_folder(ctx: &Ctx, name: &str) -> Result<PathBuf, String> {
    let folder = ctx.root.parent().ok_or("the suite root has no parent")?.join(name);
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir(&folder).map_err(|e| format!("cannot create {folder:?}: {e}"))?;
    Ok(folder)
}

/// A 64 KiB placeholder `name` in `folder`, filled by an open from another
/// process — so through the helper, which then ignore-marks it. Returns the
/// path, its inode and the payload. Fails unless the ignore mark is really
/// there afterwards, since every scenario using this is about that mark.
fn hydrated_through_open(
    ctx: &Ctx,
    folder: &Path,
    name: &str,
    item_id: &str,
) -> Result<(PathBuf, u64, Vec<u8>), String> {
    let payload: Vec<u8> = (0..(64usize * 1024)).map(|i| (i % 239) as u8 + 1).collect();
    std::fs::write(ctx.source_dir.join(item_id), &payload)
        .map_err(|e| format!("cannot write the payload: {e}"))?;
    let dir = File::open(folder).map_err(|e| e.to_string())?;
    create_placeholder(
        &dir,
        name,
        item_id,
        payload.len() as u64,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    )
    .map_err(|e| format!("cannot create the placeholder: {e}"))?;
    drop(dir);
    let path = folder.join(name);
    let ino = ctx.ino_of(&path)?;
    if ctx.read(&path)? != payload {
        return Err("the first open did not fill the file".into());
    }
    if !ignore_mark_present(ctx.helper_pid(), ino) {
        return Err("no ignore mark after the hydration, so nothing below would be tested".into());
    }
    Ok((path, ino, payload))
}

/// `Ok`, or the refusal's own name, for a trace line.
fn outcome<T>(result: &Result<T, SyncError>) -> String {
    match result {
        Ok(_) => "Ok".into(),
        Err(e) => format!("{e:?}"),
    }
}

/// What a reader in another process gets from `path`, for a trace line, and
/// whether it got exactly `payload`.
fn read_for_trace(ctx: &Ctx, path: &Path, payload: &[u8]) -> Result<(String, bool), String> {
    let before = ctx.fetches();
    let reader = Reader::start(&ctx.exe, path)?;
    let got = reader.get(Duration::from_secs(60))?;
    let fetched = ctx.fetches() - before;
    Ok(match got {
        Ok(content) => {
            let zeros = content.iter().filter(|b| **b == 0).count();
            (
                format!(
                    "a reader right away got {} bytes, {zeros} of them zero, after {fetched} \
                     fetch(es)",
                    content.len()
                ),
                content == payload,
            )
        }
        Err(errno) => (format!("a reader right away failed with errno {errno}"), false),
    })
}

/// Small round 3 measured, with its throwaway: a Forget with no
/// helper link returned `Ok` and never told the helper, so `roots.json` kept
/// naming the folder and the folder kept its directory marks and every
/// hydrated file's ignore mark; registered again without interception and
/// freed up with no link, the file was punched while still ignored, and a
/// reader right away got 65536 zero bytes after no fetch. An intercepted
/// folder's lifecycle now goes through the helper or not at all: the Forget
/// is refused `NoHelper`, and every step after it runs into a refusal of
/// its own, so the reader at the end gets the file.
fn forget_without_link_refused(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "forget-offline")?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let result = forget_without_link_steps(ctx, checks, &service, &folder);

    // Whatever happened, the helper must not keep this folder: forgotten
    // through the link, under whichever registration the daemon ended with.
    service.set_link(Some(link.clone()));
    if service.root().is_some() {
        let _ = ctx.runtime.block_on(service.unregister_root());
    }
    drop(service);
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    result
}

/// Tells the helper to drop `folder`'s registration under the id the folder
/// carries, whatever the daemon under test did. A scenario that goes red can
/// leave the helper holding a folder the daemon no longer knows — that is
/// what these scenarios are about — and on ext4 the next folder created
/// reuses the removed one's inode number, which the helper then refuses as
/// the same directory (`EINVAL`), turning one red scenario into two.
fn release_at_the_helper(ctx: &Ctx, link: &HelperLink, folder: &Path) {
    if let Ok(Some(id)) = xattr::get(folder, "user.konedrive.root") {
        if let Ok(id) = String::from_utf8(id) {
            let _ = ctx.runtime.block_on(link.unregister_root(&id));
        }
    }
}

fn forget_without_link_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    service: &SyncService,
    folder: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let folder_ino = ctx.ino_of(folder)?;
    let mut trace: Vec<String> = Vec::new();

    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    let root_id = service.root().map(|r| r.root_id).unwrap_or_default();
    let (path, ino, payload) =
        hydrated_through_open(ctx, folder, "kept.bin", "ITEM_FORGET_OFFLINE")?;
    trace.push("hydrated, ignore mark present".into());

    // The state `supervise_helper` leaves the service in while the helper is
    // away, and `main.rs` starts it in.
    service.set_link(None);
    let forgot = ctx.runtime.block_on(service.unregister_root());
    let named = std::fs::read_to_string(ROOTS_FILE).unwrap_or_default().contains(&root_id);
    trace.push(format!(
        "Forget with no link → {}; roots.json {} the root; directory mark {}; ignore mark {}",
        outcome(&forgot),
        if named { "still names" } else { "no longer names" },
        present(dir_mark_present(pid, folder_ino)),
        present(ignore_mark_present(pid, ino)),
    ));

    // The rest of round 3's measurement, in its order and still with no link.
    let reregistered = ctx.runtime.block_on(service.register_root_without_interception(folder));
    trace.push(format!("RegisterRootWithoutInterception → {}", outcome(&reregistered)));
    let dehydrated = ctx.runtime.block_on(service.dehydrate(&path));
    trace.push(format!(
        "Dehydrate → {}; {} bytes allocated; state {:?}; ignore mark {}",
        outcome(&dehydrated),
        ctx.blocks_of(&path)? * 512,
        ctx.state_of(&path)?,
        present(ignore_mark_present(pid, ino)),
    ));
    let (read, intact) = read_for_trace(ctx, &path, &payload)?;
    trace.push(read);
    checks.note(ctx.fs, "forget without a link", &trace.join("; "));

    if !matches!(forgot, Err(SyncError::NoHelper)) {
        return Err(format!(
            "{}. A Forget of an intercepted folder with no helper link must be refused \
             NoHelper: the helper was never told, so the folder stays marked and its files \
             stay ignore-marked",
            trace.join("; ")
        ));
    }
    if service.root().is_none() {
        return Err(format!("{}; the refused Forget forgot the folder anyway", trace.join("; ")));
    }
    if !intact {
        return Err(format!("{}. The reader did not get the file's content", trace.join("; ")));
    }
    Ok(())
}

/// The route into no-interception mode that H133 alone leaves open. An
/// intercepted root restored from `config.toml` used to exist nowhere in the
/// daemon until the helper came back: `resume` returned early, the daemon
/// held no root, and `RegisterRootWithoutInterception` of that same folder
/// was accepted — with the helper still holding it, its directory marks and
/// its ignore marks. The restart here is a second `SyncService` on the same
/// config file with no link, which is exactly what `main.rs` builds before
/// the supervisor's first connect. The first registration is made before
/// `resume` has run at all, which is the order a D-Bus-activated first call
/// can arrive in (zbus claims the name before `main` gets to `resume`); the
/// second after it.
fn pending_root_not_downgraded(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "restarted")?;
    let config_dir = PathBuf::from(format!("/run/konedrive-scenario-config-{}", ctx.fs));
    let _ = std::fs::remove_dir_all(&config_dir);
    let config = config_dir.join("config.toml");
    let link = ctx.link()?;
    let restarted = SyncService::new(None, None, Some(config.clone()));
    let result = pending_root_steps(ctx, checks, &link, &restarted, &config, &folder);

    // The folder is forgotten through the helper, under whatever the
    // restarted daemon ended up holding it as.
    restarted.set_link(Some(link.clone()));
    if restarted.root().is_some() {
        let _ = ctx.runtime.block_on(restarted.unregister_root());
    }
    drop(restarted);
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&config_dir);
    result
}

fn pending_root_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    link: &HelperLink,
    restarted: &SyncService,
    config: &Path,
    folder: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let mut trace: Vec<String> = Vec::new();

    {
        // The daemon before the restart: registers the folder, and then
        // simply stops — no Forget, as with a logout or a crash.
        let before = SyncService::new(Some(link.clone()), None, Some(config.to_path_buf()));
        ctx.runtime
            .block_on(before.register_root(folder))
            .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    }
    let (path, ino, payload) = hydrated_through_open(ctx, folder, "kept.bin", "ITEM_RESTARTED")?;
    trace.push("registered with interception, hydrated, ignore mark present".into());

    let early = ctx.runtime.block_on(restarted.register_root_without_interception(folder));
    trace.push(format!(
        "restarted with no link; RegisterRootWithoutInterception before resume → {}",
        outcome(&early)
    ));
    ctx.runtime.block_on(restarted.resume());
    trace.push(format!(
        "after resume: RootState {}, RootPath {:?}",
        restarted.root_state(),
        restarted.root().map(|r| r.path).unwrap_or_default()
    ));
    let reregistered = ctx.runtime.block_on(restarted.register_root_without_interception(folder));
    trace.push(format!("RegisterRootWithoutInterception → {}", outcome(&reregistered)));
    let dehydrated = ctx.runtime.block_on(restarted.dehydrate(&path));
    trace.push(format!(
        "Dehydrate → {}; {} bytes allocated; ignore mark {}",
        outcome(&dehydrated),
        ctx.blocks_of(&path)? * 512,
        present(ignore_mark_present(pid, ino)),
    ));
    let (read, intact) = read_for_trace(ctx, &path, &payload)?;
    trace.push(read);
    checks.note(ctx.fs, "restored root", &trace.join("; "));

    if !matches!(early, Err(SyncError::AlreadyRegistered))
        || !matches!(reregistered, Err(SyncError::AlreadyRegistered))
    {
        return Err(format!(
            "{}. A folder the daemon holds as intercepted — restored from config.toml, waiting \
             for its helper — must not be registered again without interception, before \
             resume or after it",
            trace.join("; ")
        ));
    }
    if !intact {
        return Err(format!("{}. The reader did not get the file's content", trace.join("; ")));
    }
    Ok(())
}

/// A no-interception folder was never announced to the helper,
/// so a Forget has nothing to tell it — and telling it anyway made the
/// folder impossible to forget while a helper was connected: measured in
/// small round 3, the helper answered `EPERM` (the root is not the uid's)
/// and the daemon kept the registration. The helper's log is the witness
/// that it was not asked at all: its refusal names the root id.
fn no_interception_forget_is_local(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "unintercepted-forget")?;
    let service = SyncService::new(Some(ctx.link()?), None, None);
    let log = ctx.helper.lock().unwrap().log.clone();
    let result = (|| -> Result<(), String> {
        ctx.runtime
            .block_on(service.register_root_without_interception(&folder))
            .map_err(|e| format!("cannot register {folder:?} without interception: {e}"))?;
        let root_id = service.root().map(|r| r.root_id).unwrap_or_default();
        let forgot = ctx.runtime.block_on(service.unregister_root());
        let named = count_in_log(&log, &root_id);
        let trace = format!(
            "Forget with the helper connected → {}; the daemon {} the folder; the helper's log \
             names its root id {named} time(s)",
            outcome(&forgot),
            if service.root().is_some() { "still holds" } else { "no longer holds" },
        );
        checks.note(ctx.fs, "no-interception forget", &trace);
        if forgot.is_err() || service.root().is_some() {
            return Err(format!(
                "{trace}. A folder registered without interception must be forgettable while a \
                 helper is connected"
            ));
        }
        if named > 0 {
            return Err(format!("{trace}. The helper was asked about a root it never registered"));
        }
        Ok(())
    })();
    drop(service);
    let _ = std::fs::remove_dir_all(&folder);
    result
}

/// A folder registered without interception is never announced
/// to the helper, and `PopulateFromDirectory` marks every directory it
/// creates — which it used to do whenever a link existed, in a
/// no-interception folder too. The helper authorises a mark by *device*, so
/// on a filesystem where the uid owns any root (here: the suite's own) the
/// mark lands and the directory is intercepted, in a folder the user asked
/// to leave alone. A reader of a placeholder under the new directory must
/// not be intercepted at all: no mark, no fetch. (What a punch there does
/// about an ignore mark no longer depends on this: local rule
/// has it clear the mark whenever there is a link.)
fn no_interception_populate_marks_nothing(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "unintercepted-populate")?;
    let source = scenario_folder(ctx, "unintercepted-populate-source")?;
    let service = SyncService::new(Some(ctx.link()?), None, None);
    let result = (|| -> Result<(), String> {
        std::fs::create_dir(source.join("sub")).map_err(|e| e.to_string())?;
        std::fs::write(source.join("sub/inner.bin"), vec![5u8; 8192]).map_err(|e| e.to_string())?;
        ctx.runtime
            .block_on(service.register_root_without_interception(&folder))
            .map_err(|e| format!("cannot register {folder:?} without interception: {e}"))?;
        let populated = ctx.runtime.block_on(service.populate_from_directory(&source));
        let sub = folder.join("sub");
        let marked = sub.exists() && dir_mark_present(ctx.helper_pid(), ctx.ino_of(&sub)?);
        let before = ctx.fetches();
        let read = match Reader::start(&ctx.exe, &sub.join("inner.bin"))?.get(Duration::from_secs(60))? {
            Ok(content) => format!("{} bytes", content.len()),
            Err(errno) => format!("errno {errno}"),
        };
        let fetched = ctx.fetches() - before;
        let trace = format!(
            "populate with the helper connected → {}; sub/ directory mark {}; a reader of \
             sub/inner.bin got {read} after {fetched} fetch(es)",
            outcome(&populated),
            present(marked),
        );
        checks.note(ctx.fs, "no-interception populate", &trace);
        populated.map_err(|e| format!("{trace}; populate failed: {e}"))?;
        if marked || fetched > 0 {
            return Err(format!(
                "{trace}. A directory in a no-interception folder was marked: opens under it \
                 are intercepted in a folder registered without interception"
            ));
        }
        Ok(())
    })();
    drop(service);
    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&source);
    result
}

/// A filesystem of the type under test that the helper holds no root on:
/// where the helper's device-scoped check refuses every mark request from
/// this uid, which is the ordinary state of a machine whose only folder is
/// registered without interception. In the suite itself, uid 0 always owns
/// the suite root on the filesystem under test, so the helper allows what it
/// would refuse there.
struct ScratchFs {
    image: PathBuf,
    mount: PathBuf,
}

impl ScratchFs {
    fn create(fs: &'static str, name: &str) -> Result<Self, String> {
        let image = PathBuf::from(format!("/mnt/img/{name}-{fs}.img"));
        let mount = PathBuf::from(format!("/mnt/{name}-{fs}"));
        let _ = Command::new("umount").arg(&mount).stderr(Stdio::null()).status();
        let _ = std::fs::remove_file(&image);
        std::fs::create_dir_all(&mount).map_err(|e| format!("cannot create {mount:?}: {e}"))?;
        let run = |program: &str, args: &[&str]| -> Result<(), String> {
            let out = Command::new(program)
                .args(args)
                .output()
                .map_err(|e| format!("cannot run {program}: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "{program} {args:?} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(())
        };
        let image_arg = image.to_str().ok_or("non-UTF-8 image path")?;
        let mount_arg = mount.to_str().ok_or("non-UTF-8 mount path")?;
        // 512 MiB, sparse on the guest's tmpfs: above XFS's 300 MiB minimum.
        run("truncate", &["-s", "512M", image_arg])?;
        match fs {
            "btrfs" => run("mkfs.btrfs", &["-q", "-f", image_arg])?,
            "ext4" => run("mkfs.ext4", &["-q", "-F", image_arg])?,
            "xfs" => run("mkfs.xfs", &["-q", "-f", image_arg])?,
            other => return Err(format!("no scratch filesystem for {other}")),
        }
        run("mount", &["-o", "loop", image_arg, mount_arg])?;
        let scratch = ScratchFs { image, mount };
        let expected = FILESYSTEMS.iter().find(|(name, _)| *name == fs).map(|(_, m)| *m);
        let found = statfs_type(&scratch.mount)?;
        if Some(found) != expected {
            return Err(format!(
                "{} reports f_type {found:#x}, not the {fs} it should",
                scratch.mount.display()
            ));
        }
        Ok(scratch)
    }
}

impl Drop for ScratchFs {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mount).status();
        let _ = std::fs::remove_file(&self.image);
        let _ = std::fs::remove_dir(&self.mount);
    }
}

/// Where it decides whether the mode works at all. On a
/// filesystem where the uid owns no helper root, the helper refuses a
/// `ClearIgnore` — and a `MarkDir` — with `EPERM`. Sending either for a
/// no-interception folder therefore failed every dehydration there, and
/// every populate of a tree with a subdirectory in it, the moment a helper
/// happened to be connected. That was reasoned in small round 3; this
/// measures it. Nothing in such a folder was ever announced to the helper,
/// so nothing is asked of it now.
fn no_interception_with_helper_connected(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let scratch = ScratchFs::create(ctx.fs, "unowned")?;
    let folder = scratch.mount.join("root");
    let source = scratch.mount.join("source");
    let service = SyncService::new(Some(ctx.link()?), None, None);
    let result = (|| -> Result<(), String> {
        std::fs::create_dir(&folder).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(source.join("sub")).map_err(|e| e.to_string())?;
        let payload: Vec<u8> = (0..(64usize * 1024)).map(|i| (i % 233) as u8 + 1).collect();
        std::fs::write(source.join("sub/doc.bin"), &payload).map_err(|e| e.to_string())?;
        let mut trace: Vec<String> = Vec::new();

        ctx.runtime
            .block_on(service.register_root_without_interception(&folder))
            .map_err(|e| format!("cannot register {folder:?} without interception: {e}"))?;
        let populated = ctx.runtime.block_on(service.populate_from_directory(&source));
        trace.push(format!("populate → {}", outcome(&populated)));
        let file = folder.join("sub/doc.bin");
        let not_reached = || "not reached".to_string();
        let hydrated =
            populated.is_ok().then(|| ctx.runtime.block_on(service.hydrate_now(&file)));
        trace.push(format!("Hydrate → {}", hydrated.as_ref().map_or_else(not_reached, outcome)));
        let dehydrated = matches!(hydrated, Some(Ok(())))
            .then(|| ctx.runtime.block_on(service.dehydrate(&file)));
        trace.push(format!(
            "Dehydrate with the helper connected → {}",
            dehydrated.as_ref().map_or_else(not_reached, outcome)
        ));
        let forgot = ctx.runtime.block_on(service.unregister_root());
        trace.push(format!("Forget → {}", outcome(&forgot)));
        checks.note(ctx.fs, "no-interception with a helper", &trace.join("; "));

        if populated.is_err() || !matches!(dehydrated, Some(Ok(()))) || forgot.is_err() {
            return Err(format!(
                "{}. A folder registered without interception must work with a helper \
                 connected, on a filesystem where the helper holds no root of this uid",
                trace.join("; ")
            ));
        }
        ctx.holds_no_data(&file).map_err(|e| format!("after the dehydration, {e}"))?;
        match ctx.state_of(&file)? {
            Some(State::OnlineOnly) => Ok(()),
            other => Err(format!("the state after the dehydration is {other:?}")),
        }
    })();
    drop(service);
    drop(scratch);
    result
}

/// Found in real use: a folder registered while the helper
/// was not running ("Use Without the Helper") stayed without interception
/// once the helper was installed, and every file in it read as zeros until a
/// Forget and a new registration. Here the helper is really stopped while the
/// folder is registered and filled, and a reader proves the placeholder reads
/// as zeros then. The helper is started again, and the daemon's own
/// supervisor — on a runtime of its own, so that its connection goes with it
/// at the end — connects, switches the folder to interception (the helper's
/// registration walk marks its directories), and serves the fill: a reader in
/// another process gets the file's content.
fn upgraded_when_the_helper_starts(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "upgraded")?;
    let source = scenario_folder(ctx, "upgraded-source")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot build a runtime: {e}"))?;
    let service = SyncService::new(None, None, None);
    let result = upgraded_steps(ctx, checks, &runtime, &service, &folder, &source);

    // Forgotten through the helper, under whatever the daemon ended up
    // holding it as; then this daemon's connection is closed, so that
    // hydrations go back to the suite's own daemon.
    if service.root().is_some() && service.link().is_some() {
        let _ = runtime.block_on(service.unregister_root());
    }
    service.set_link(None);
    runtime.shutdown_timeout(Duration::from_secs(5));
    drop(service);
    if !ctx.helper_alive() || !ctx.daemon_connected() {
        let _ = ctx.restart_helper();
    }
    if let Ok(link) = ctx.link() {
        release_at_the_helper(ctx, &link, &folder);
    }
    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&source);
    result
}

fn upgraded_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    runtime: &tokio::runtime::Runtime,
    service: &Arc<SyncService>,
    folder: &Path,
    source: &Path,
) -> Result<(), String> {
    let payload: Vec<u8> = (0..(64usize * 1024)).map(|i| (i % 229) as u8 + 1).collect();
    std::fs::create_dir(source.join("sub")).map_err(|e| e.to_string())?;
    std::fs::write(source.join("sub/doc.bin"), &payload).map_err(|e| e.to_string())?;
    let file = folder.join("sub/doc.bin");
    let mut trace: Vec<String> = Vec::new();

    // The machine before the helper is installed: no helper running at all.
    ctx.kill_daemon();
    ctx.helper.lock().unwrap().stop();
    runtime
        .block_on(service.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register {folder:?} without interception: {e}"))?;
    let placed = runtime
        .block_on(service.populate_from_directory(source))
        .map_err(|e| format!("cannot populate {folder:?}: {e}"))?;
    let before = Reader::start(&ctx.exe, &file)?.get(Duration::from_secs(30))?;
    trace.push(format!(
        "no helper running: registered without interception, {placed} placeholder(s), RootState \
         {}; a reader got {}",
        service.root_state(),
        got(&before, &payload)
    ));

    // The helper is installed and started. The suite's own daemon connects
    // first, so that this one's connection is the newest and the fill comes
    // here, where the payload is.
    ctx.restart_helper()?;
    let supervisor = runtime.spawn(supervise_helper(
        Arc::clone(service),
        PathBuf::from(SOCKET_PATH),
        Duration::from_millis(50),
    ));
    let deadline = Instant::now() + Duration::from_secs(30);
    while service.root_state() != "ready" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let sub_marked = dir_mark_present(ctx.helper_pid(), ctx.ino_of(&folder.join("sub"))?);
    trace.push(format!(
        "the helper started: RootState {}, LastError {:?}, sub/ directory mark {}",
        service.root_state(),
        service.last_error(),
        present(sub_marked)
    ));
    let after = Reader::start(&ctx.exe, &file)?.get(Duration::from_secs(60))?;
    let state = ctx.state_of(&file)?;
    trace.push(format!("a reader got {}; the file is {state:?}", got(&after, &payload)));
    supervisor.abort();
    checks.note(ctx.fs, "helper arrives", &trace.join("; "));

    if !all_zeros(before.as_deref().unwrap_or_default(), payload.len()) {
        return Err(format!(
            "{}. Before the helper ran, the placeholder must read as zeros, or this does not \
             reproduce the defect at all",
            trace.join("; ")
        ));
    }
    if service.root_state() != "ready" || !sub_marked {
        return Err(format!(
            "{}. The folder was not switched to interception when the helper arrived",
            trace.join("; ")
        ));
    }
    if after.as_deref() != Ok(payload.as_slice()) || state != Some(State::Hydrated) {
        return Err(format!("{}. The reader did not get the file's content", trace.join("; ")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Probes C1, C2, I1, I2, kept as regression scenarios
// ---------------------------------------------------------------------------

/// Whether `content` is `len` bytes of nothing but zeros: what an opener let
/// through onto an unfilled placeholder reads.
fn all_zeros(content: &[u8], len: usize) -> bool {
    content.len() == len && content.iter().all(|&b| b == 0)
}

/// What a reader got, for a trace line.
fn got(result: &Result<Vec<u8>, i32>, payload: &[u8]) -> String {
    match result {
        Ok(content) if content == payload => format!("the {} bytes it should", content.len()),
        Ok(content) if all_zeros(content, payload.len()) => {
            format!("{} bytes, ALL ZERO", content.len())
        }
        Ok(content) => format!("{} bytes, not the payload", content.len()),
        Err(errno) => format!("errno {errno}"),
    }
}

/// C1. A hydration request enrolled while X
/// was `online-only` reaches a fill slot only after X was filled directly —
/// what `Hydrate()` does — and an opener in between found X `hydrated` and
/// had the helper ignore-mark it. Before the fix, the stale request filled X
/// again without looking: it wrote `hydrating` over a hydrated file, fetched,
/// and on a failed fetch rolled back — demoting X to `online-only` and
/// punching it — while the ignore mark the second opener left stayed on. The
/// next reader was never intercepted and got 65 536 zero bytes after no fetch,
/// on all three filesystems, with nothing injected.
///
/// Under the per-inode lock the fill must look at the state again, and a file
/// that is already `hydrated` is answered at once, untouched.
fn stale_request_after_direct_fill(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let mut trace: Vec<String> = Vec::new();
    let payload: Vec<u8> = (0..65536usize).map(|i| (i % 251) as u8 + 1).collect();
    let x = ctx.place("stale/x.bin", "ITEM_STALE_X", &payload)?;
    let ino = ctx.ino_of(&x)?;
    let mut ys = Vec::new();
    for i in 0..4u8 {
        ys.push(ctx.place(&format!("stale/y{i}.bin"), &format!("ITEM_STALE_Y{i}"), &[i + 1; 4096])?);
    }

    // Four slow fills take every one of the daemon's fill slots.
    ctx.set_source_delay(Duration::from_millis(6000));
    let before = ctx.fetches();
    let y_readers =
        ys.iter().map(|y| Reader::start(&ctx.exe, y)).collect::<Result<Vec<_>, _>>()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ctx.fetches() < before + 4 {
        if Instant::now() > deadline {
            return Err("the four slow fills never started".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Reader A: its request reaches the daemon and waits there for a slot.
    let a = Reader::start(&ctx.exe, &x)?;
    std::thread::sleep(Duration::from_millis(500));
    ctx.set_source_delay(Duration::from_millis(0));

    // X filled directly, as `SyncService::hydrate_now` does it: an exempt
    // open, the per-inode lock, `source::hydrate`.
    {
        let file = File::options().read(true).write(true).open(&x).map_err(|e| e.to_string())?;
        let key = InodeKey::of(&file).map_err(|e| e.to_string())?;
        let locks = ctx.locks.clone();
        let source = Arc::clone(&ctx.source) as Arc<dyn ContentSource>;
        let errno = ctx.runtime.block_on(async move {
            let _guard = locks.lock(key).await;
            konedrived::sync::source::hydrate(file.into(), source.as_ref()).await
        });
        trace.push(format!("X filled directly (errno {errno}), state {:?}", ctx.state_of(&x)?));
    }

    // Reader B finds X hydrated: allowed, and X is ignore-marked.
    let b = ctx.read(&x)?;
    trace.push(format!(
        "reader B got {}; ignore mark {}",
        got(&Ok(b), &payload),
        present(ignore_mark_present(ctx.helper_pid(), ino))
    ));

    // From here on a fetch of X fails.
    std::fs::remove_file(ctx.source_dir.join("ITEM_STALE_X")).map_err(|e| e.to_string())?;
    for reader in &y_readers {
        let _ = reader.get(Duration::from_secs(20));
    }
    let a_result = a.get(Duration::from_secs(20))?;
    trace.push(format!("reader A, whose request had waited, got {}", got(&a_result, &payload)));
    let after_a = ctx.state_of(&x)?;
    trace.push(format!(
        "X now: state {after_a:?}, {} bytes allocated, ignore mark {}",
        ctx.blocks_of(&x)? * 512,
        present(ignore_mark_present(ctx.helper_pid(), ino))
    ));

    let f0 = ctx.fetches();
    let c = Reader::start(&ctx.exe, &x)?.get(Duration::from_secs(60))?;
    trace.push(format!("reader C got {} after {} fetch(es)", got(&c, &payload), ctx.fetches() - f0));
    checks.note(ctx.fs, "stale request", &trace.join("; "));

    if !matches!(&c, Ok(content) if *content == payload) {
        return Err(format!("{}. A reader did not get the file's content", trace.join("; ")));
    }
    if !matches!(&a_result, Ok(content) if *content == payload) {
        return Err(format!(
            "{}. The request that waited found X already filled and must have been answered \
             with it, untouched",
            trace.join("; ")
        ));
    }
    if after_a != Some(State::Hydrated) {
        return Err(format!("{}. X must still be hydrated", trace.join("; ")));
    }
    Ok(())
}

/// C2. A hydration still in flight when its
/// folder is forgotten finishes *after* `UnregisterRoot`'s walk has passed the
/// file, and the helper ignore-marks it then. Before the fix the mark stayed:
/// the folder was registered without interception — which by design sends no
/// `ClearIgnore` — freed up, forgotten, and registered with interception
/// again, and the reader got 65 536 zero bytes after no fetch, on all three
/// filesystems, with the helper connected throughout and nothing injected.
///
/// Since the free-up itself has the helper clear the mark first,
/// whatever the folder's mode, so an emptied file never carries one; the
/// registration walk's clearing, and the helper's refusal to mark a file for
/// a hydration that began before an unregistration, stay as defence in
/// depth — the second is asserted here on its own.
fn inflight_across_forget(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "inflight-forget")?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let result = inflight_across_forget_steps(ctx, checks, &service, &folder);

    ctx.set_source_delay(Duration::from_millis(0));
    service.set_link(Some(link.clone()));
    if service.root().is_some() {
        let _ = ctx.runtime.block_on(service.unregister_root());
    }
    drop(service);
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    result
}

fn inflight_across_forget_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    service: &SyncService,
    folder: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let mut trace: Vec<String> = Vec::new();
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    let payload: Vec<u8> = (0..(64usize * 1024)).map(|i| (i % 233) as u8 + 1).collect();
    std::fs::write(ctx.source_dir.join("ITEM_INFLIGHT"), &payload).map_err(|e| e.to_string())?;
    let dir = File::open(folder).map_err(|e| e.to_string())?;
    create_placeholder(
        &dir,
        "x.bin",
        "ITEM_INFLIGHT",
        payload.len() as u64,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    )
    .map_err(|e| e.to_string())?;
    drop(dir);
    let x = folder.join("x.bin");
    let ino = ctx.ino_of(&x)?;

    ctx.set_source_delay(Duration::from_millis(3000));
    let before = ctx.fetches();
    let a = Reader::start(&ctx.exe, &x)?;
    ctx.wait_for_fetch(before, Duration::from_secs(10))?;
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    trace.push(format!("Forget mid-fill; ignore mark {}", present(ignore_mark_present(pid, ino))));
    let a_result = a.get(Duration::from_secs(20))?;
    ctx.set_source_delay(Duration::from_millis(0));
    let marked_after_fill = ignore_mark_present(pid, ino);
    trace.push(format!(
        "the fill finished, reader A got {}; ignore mark {}",
        got(&a_result, &payload),
        present(marked_after_fill)
    ));

    ctx.runtime
        .block_on(service.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register without interception: {e}"))?;
    ctx.runtime.block_on(service.dehydrate(&x)).map_err(|e| format!("Dehydrate: {e}"))?;
    trace.push(format!(
        "freed up without interception: {} bytes allocated, state {:?}, ignore mark {}",
        ctx.blocks_of(&x)? * 512,
        ctx.state_of(&x)?,
        present(ignore_mark_present(pid, ino))
    ));
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register with interception again: {e}"))?;
    trace.push(format!(
        "registered with interception again: ignore mark {}",
        present(ignore_mark_present(pid, ino))
    ));

    let f0 = ctx.fetches();
    let c = Reader::start(&ctx.exe, &x)?.get(Duration::from_secs(60))?;
    trace.push(format!("reader C got {} after {} fetch(es)", got(&c, &payload), ctx.fetches() - f0));
    checks.note(ctx.fs, "in flight across a Forget", &trace.join("; "));
    if !matches!(&c, Ok(content) if *content == payload) {
        return Err(format!(
            "{}. A reader of a file freed up while its folder was not intercepted must be \
             intercepted once the folder is registered with interception again",
            trace.join("; ")
        ));
    }
    if !matches!(&a_result, Ok(content) if *content == payload) {
        return Err(format!("{}. Reader A did not get the file", trace.join("; ")));
    }
    if marked_after_fill {
        return Err(format!(
            "{}. A hydration that began before its folder was forgotten must not leave an ignore \
             mark behind the unregistration's walk (Ruling H138's second guard)",
            trace.join("; ")
        ));
    }
    Ok(())
}

/// The registration walk on its own, with no race in it. A file
/// hydrated in an intercepted folder keeps its ignore mark when it is moved
/// out of the folder — the mark is on the inode — so the unregistration walk
/// never meets it. Moved back in after the folder was forgotten, and freed up
/// while the folder is registered without interception (which sends no
/// `ClearIgnore`), it is empty and still ignore-marked when the folder is
/// registered with interception again. Only the registration walk can clear
/// it there, and before that walk cleared ignore marks the reader got zeros.
fn carried_in_ignore_mark(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "carried-in")?;
    let aside = scenario_folder(ctx, "carried-in-aside")?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let result = carried_in_steps(ctx, checks, &service, &folder, &aside);

    service.set_link(Some(link.clone()));
    if service.root().is_some() {
        let _ = ctx.runtime.block_on(service.unregister_root());
    }
    drop(service);
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&aside);
    result
}

fn carried_in_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    service: &SyncService,
    folder: &Path,
    aside: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let mut trace: Vec<String> = Vec::new();
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    let (path, ino, payload) = hydrated_through_open(ctx, folder, "kept.bin", "ITEM_CARRIED")?;
    let away = aside.join("kept.bin");
    std::fs::rename(&path, &away).map_err(|e| format!("cannot move the file out: {e}"))?;
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    trace.push(format!(
        "hydrated, moved out, folder forgotten: ignore mark {}",
        present(ignore_mark_present(pid, ino))
    ));
    std::fs::rename(&away, &path).map_err(|e| format!("cannot move the file back: {e}"))?;
    ctx.runtime
        .block_on(service.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register without interception: {e}"))?;
    ctx.runtime.block_on(service.dehydrate(&path)).map_err(|e| format!("Dehydrate: {e}"))?;
    trace.push(format!(
        "moved back and freed up without interception: {} bytes allocated, state {:?}, ignore \
         mark {}",
        ctx.blocks_of(&path)? * 512,
        ctx.state_of(&path)?,
        present(ignore_mark_present(pid, ino))
    ));
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register with interception again: {e}"))?;
    trace.push(format!(
        "registered with interception again: ignore mark {}",
        present(ignore_mark_present(pid, ino))
    ));
    let f0 = ctx.fetches();
    let c = Reader::start(&ctx.exe, &path)?.get(Duration::from_secs(60))?;
    trace.push(format!("a reader got {} after {} fetch(es)", got(&c, &payload), ctx.fetches() - f0));
    checks.note(ctx.fs, "carried in", &trace.join("; "));
    if !matches!(&c, Ok(content) if *content == payload) {
        return Err(format!(
            "{}. Registering a folder with interception must clear the ignore mark of every \
             file in it",
            trace.join("; ")
        ));
    }
    Ok(())
}

/// I1. The helper's "read `hydrated`, then
/// place the ignore mark" was two steps, and nothing ordered them against a
/// dehydration's `dehydrating` + `ClearIgnore`. A worker that read `hydrated`
/// before the dehydration began and marked after its `ClearIgnore` left a
/// mark the punch then outlived. The natural window is under 100 µs, so a
/// stall is injected there (`KONEDRIVE_FAULT_DELAY_IGNORE_MS`, compiled only
/// under `fault-injection`); the dehydration runs `root::dehydrate_opened`'s
/// steps in its order, under the per-inode lock `SyncService::dehydrate`
/// holds, with the gap before the lease made long enough for the stalled
/// worker to act in it.
///
/// Before the fix the worker marked the file and let the opener through; the
/// lease was granted, the file was punched with the mark on it, and the next
/// reader got 65 536 zero bytes after no fetch. With the mark read back
/// against the state, the worker sees `dehydrating`, takes the mark off again
/// and asks for a hydration; its event descriptor then refuses the lease, the
/// dehydration rolls back, and every reader gets the file.
fn late_ignore_mark(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    ctx.restart_helper_with(None, Some(("KONEDRIVE_FAULT_DELAY_IGNORE_MS", "1500")))?;
    let result = late_ignore_mark_steps(ctx, checks);
    ctx.restart_helper_with(None, None)?;
    result
}

fn late_ignore_mark_steps(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    use konedrive_fs::lease::WriteLease;
    use konedrive_fs::placeholder::{punch_all, remove_stamp};

    let mut trace: Vec<String> = Vec::new();
    let pid = ctx.helper_pid();
    let payload: Vec<u8> = (0..65536usize).map(|i| (i % 227) as u8 + 1).collect();
    let x = ctx.place("late/x.bin", "ITEM_LATE_X", &payload)?;
    let ino = ctx.ino_of(&x)?;
    if ctx.read(&x)? != payload {
        return Err("the first open did not fill the file".into());
    }

    // Our own open is the exempt daemon's. The mark comes off, so that the
    // next open reaches the helper's `hydrated` arm, and stalls there.
    let file = File::options().read(true).write(true).open(&x).map_err(|e| e.to_string())?;
    let key = InodeKey::of(&file).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    ctx.runtime.block_on(link.clear_ignore(&file)).map_err(|e| e.to_string())?;
    let b = Reader::start(&ctx.exe, &x)?;
    std::thread::sleep(Duration::from_millis(300));

    // The dehydration, in `dehydrate_opened`'s order, under the lock.
    let guard = ctx.runtime.block_on(ctx.locks.lock(key));
    write_state(&file, State::Dehydrating).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    ctx.runtime.block_on(link.clear_ignore(&file)).map_err(|e| e.to_string())?;
    trace.push("dehydrating made durable, ClearIgnore acknowledged".into());

    // The worker resumes 1.5 s after the open; give it a second more.
    let early = b.poll(Duration::from_millis(2200));
    trace.push(match &early {
        Some((result, at)) => format!("reader B got {} after {at:?}", got(result, &payload)),
        None => "reader B still suspended".into(),
    });
    let punched = match WriteLease::take(&file).map_err(|e| e.to_string())? {
        Some(lease) => {
            punch_all(&file).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            write_state(&file, State::OnlineOnly).map_err(|e| e.to_string())?;
            remove_stamp(&file).map_err(|e| e.to_string())?;
            drop(lease);
            true
        }
        None => {
            write_state(&file, State::Hydrated).map_err(|e| e.to_string())?;
            false
        }
    };
    drop(guard);
    trace.push(if punched {
        format!("lease granted, punched; ignore mark {}", present(ignore_mark_present(pid, ino)))
    } else {
        "lease refused (the file is in use), rolled back to hydrated".into()
    });
    let b_result = match early {
        Some((result, _)) => result,
        None => b.get(Duration::from_secs(20))?,
    };
    if punched {
        trace.push(format!("reader B got {}", got(&b_result, &payload)));
    }

    let f0 = ctx.fetches();
    let c = Reader::start(&ctx.exe, &x)?.get(Duration::from_secs(60))?;
    trace.push(format!("reader C got {} after {} fetch(es)", got(&c, &payload), ctx.fetches() - f0));
    checks.note(ctx.fs, "late ignore mark", &trace.join("; "));
    if !matches!(&c, Ok(content) if *content == payload)
        || !matches!(&b_result, Ok(content) if *content == payload)
    {
        return Err(format!("{}. Every reader must get the file's content", trace.join("; ")));
    }
    Ok(())
}

/// I2. fanotify creates each event's
/// descriptor inside the listener's `read()`, and opening a file somebody
/// holds a write lease on waits for the lease to break. Measured by the
/// review, and here: the helper's whole event loop stops for as long as the
/// lease is held, and every other intercepted open on the machine waits
/// behind it — every dehydration's punch does this, and any local user can,
/// for 45 s at a time.
///
/// The daemon's own open (exempt) takes a write lease on an `online-only`
/// placeholder F, exactly as a dehydration holds one across its punch. Reader
/// B then opens F, and reader C opens G, another placeholder in the same
/// folder. C must be answered at once, whatever becomes of B; and B must be
/// answered — never left suspended and never let through onto F's zeros.
///
/// Before the event descriptors were `O_NONBLOCK`, C waited 2.5 s — exactly
/// as long as the lease was held — on all three filesystems. With it, the
/// kernel cannot create B's event descriptor, answers B `FAN_DENY` itself
/// (B sees `EPERM` at once), and C is answered in milliseconds.
fn leased_file_does_not_stall_others(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    use konedrive_fs::lease::WriteLease;

    let f_payload: Vec<u8> = (0..65536usize).map(|i| (i % 211) as u8 + 1).collect();
    let g_payload: Vec<u8> = (0..65536usize).map(|i| (i % 199) as u8 + 1).collect();
    let f = ctx.place("leased/f.bin", "ITEM_LEASED_F", &f_payload)?;
    let g = ctx.place("leased/g.bin", "ITEM_LEASED_G", &g_payload)?;

    let file = File::options().read(true).write(true).open(&f).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let lease = loop {
        // The helper may still be closing the event descriptor of our own
        // open, which refuses the lease for a moment.
        if let Some(lease) = WriteLease::take(&file).map_err(|e| e.to_string())? {
            break lease;
        }
        if Instant::now() > deadline {
            return Err("could not take a write lease on the placeholder".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let log = ctx.helper.lock().unwrap().log.clone();
    let unopenable_before = occurrences_in_log(&log, UNOPENABLE);
    let b = Reader::start(&ctx.exe, &f)?;
    std::thread::sleep(Duration::from_millis(200));
    let c = Reader::start(&ctx.exe, &g)?;
    let c_early = c.poll(Duration::from_millis(2500));
    let b_early = b.poll(Duration::from_millis(0));
    drop(lease);
    drop(file);
    let (c_result, c_after) = match c_early {
        Some(done) => done,
        None => c.get_timed(Duration::from_secs(60))?,
    };
    let b_while_leased = b_early.is_some();
    let (b_result, b_after) = match b_early {
        Some(done) => done,
        None => b.get_timed(Duration::from_secs(60))?,
    };
    let f0 = ctx.fetches();
    let again = Reader::start(&ctx.exe, &f)?.get(Duration::from_secs(60))?;
    std::thread::sleep(THROTTLE_SETTLE);
    let unopenable = occurrences_in_log(&log, UNOPENABLE) - unopenable_before;
    let trace = format!(
        "with F leased: reader C of G got {} after {c_after:?}; reader B of F got {} after \
         {b_after:?}{}; the helper found the group readable and nothing to read {unopenable} \
         time(s); once the lease was released, a reader of F got {} after {} fetch(es)",
        got(&c_result, &g_payload),
        got(&b_result, &f_payload),
        if b_while_leased { " (while the lease was held)" } else { " (after the lease went)" },
        got(&again, &f_payload),
        ctx.fetches() - f0,
    );
    checks.note(ctx.fs, "lease and the event loop", &trace);
    if matches!(&b_result, Ok(content) if *content != f_payload) {
        return Err(format!("{trace}. Reader B was let through onto F without its content"));
    }
    if !matches!(&c_result, Ok(content) if *content == g_payload) {
        return Err(format!("{trace}. Reader C did not get G's content"));
    }
    if c_after > Duration::from_secs(1) {
        return Err(format!(
            "{trace}. An open of another file waited behind a lease on F: the helper's event \
             loop is frozen for as long as anybody holds a lease in a marked folder"
        ));
    }
    if !matches!(&again, Ok(content) if *content == f_payload) {
        return Err(format!("{trace}. F could not be read once the lease was released"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Probes N1, N2, N3, and the rule that replaced the
// no-interception chain
// ---------------------------------------------------------------------------

/// What the helper logs when `read()` of its group reports that the kernel
/// could not create one event's descriptor. The helper's
/// `EVENT_FD_FAILED`, in part.
const EVENT_FD_FAILED: &str = "could not open the descriptor";

/// Takes a mount back down however a scenario ends.
struct Unmount(PathBuf);

impl Drop for Unmount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// How many times the helper has said `needle` by now, waiting up to
/// [`THROTTLE_SETTLE`] for the count to move past `before`.
fn settled_count(log: &Path, needle: &str, before: usize) -> usize {
    let deadline = Instant::now() + THROTTLE_SETTLE;
    loop {
        let now = occurrences_in_log(log, needle);
        if now > before || Instant::now() > deadline {
            return now - before;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What one opener got, for a trace line: the errno and how soon, or what
/// it read.
fn answered(result: &Result<Vec<u8>, i32>, after: Duration) -> String {
    match result {
        Ok(content) if all_zeros(content, content.len()) && !content.is_empty() => {
            format!("{} bytes, ALL ZERO, after {after:?}", content.len())
        }
        Ok(content) => format!("{} bytes after {after:?}", content.len()),
        Err(errno) => format!("errno {errno} after {after:?}"),
    }
}

/// N1, first trigger. The kernel opens
/// each event's descriptor with the group's `O_RDWR` against the **opener's**
/// mount; through a read-only mount that open fails `EROFS`, and `read()` of
/// the group returns it. The helper treated anything but four errnos as
/// fatal and exited, and the kernel then allowed every suspended open: a
/// reader waiting for a slow hydration got 65 536 zero bytes. A Flatpak app
/// with `home:ro` is such an opener.
///
/// The event is one the kernel could not hand over. What the kernel does
/// with its opener is measured here: it must be answered, never allowed —
/// `target.bin` is an `online-only` placeholder, so "allowed" reads zeros.
fn readonly_mount_open_survived(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let slow_payload: Vec<u8> = (0..65536usize).map(|i| (i % 193) as u8 + 1).collect();
    let target_payload: Vec<u8> = (0..65536usize).map(|i| (i % 181) as u8 + 1).collect();
    let slow = ctx.place("rofs/slow.bin", "ITEM_RO_SLOW", &slow_payload)?;
    let target = ctx.place("rofs/target.bin", "ITEM_RO_TARGET", &target_payload)?;
    let pid = ctx.helper_pid();
    let dir_ino = ctx.ino_of(&ctx.root.join("rofs"))?;
    let log = ctx.helper.lock().unwrap().log.clone();
    let refused_before = occurrences_in_log(&log, EVENT_FD_FAILED);

    ctx.set_source_delay(Duration::from_millis(4000));
    let before = ctx.fetches();
    let b = Reader::start(&ctx.exe, &slow)?;
    ctx.wait_for_fetch(before, Duration::from_secs(10))?;

    let ro = PathBuf::from(format!("/mnt/{}-ro", ctx.fs));
    let _ = Command::new("umount").arg(&ro).stderr(Stdio::null()).status();
    std::fs::create_dir_all(&ro).map_err(|e| e.to_string())?;
    let mounted = Command::new("mount")
        .args(["--bind", "-o", "ro"])
        .arg(&ctx.root)
        .arg(&ro)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if !mounted {
        return Err("cannot make a read-only bind mount of the root".into());
    }
    let unmount = Unmount(ro.clone());
    let through = ro.join("rofs/target.bin");
    let written = std::fs::OpenOptions::new().write(true).open(&through);
    if !matches!(&written, Err(e) if e.raw_os_error() == Some(libc::EROFS)) {
        return Err(format!("{} is not read-only", ro.display()));
    }

    let f0 = ctx.fetches();
    let r = Reader::start(&ctx.exe, &through)?;
    let (r_result, r_after) = r.get_timed(Duration::from_secs(10))?;
    std::thread::sleep(Duration::from_millis(300));
    let alive = ctx.helper_alive();
    let group_kept = alive && dir_mark_present(pid, dir_ino);
    let fetched_for_ro = ctx.fetches() - f0;
    let b_result = b.get(Duration::from_secs(20))?;
    ctx.set_source_delay(Duration::from_millis(0));
    drop(unmount);

    let mut trace = vec![format!(
        "the opener through the read-only mount got {} ({fetched_for_ro} fetch(es) for it); \
         helper alive: {alive}, its directory mark {}; the reader suspended on slow.bin got {}",
        answered(&r_result, r_after),
        present(group_kept),
        got(&b_result, &slow_payload),
    )];
    if alive {
        let refused = settled_count(&log, EVENT_FD_FAILED, refused_before);
        let f1 = ctx.fetches();
        let again = Reader::start(&ctx.exe, &target)?.get(Duration::from_secs(60))?;
        trace.push(format!(
            "the helper logged {refused} event(s) it could not be handed; the same file through \
             the ordinary path then got {} after {} fetch(es)",
            got(&again, &target_payload),
            ctx.fetches() - f1
        ));
        checks.note(ctx.fs, "read-only mount", &trace.join("; "));
        if !matches!(&again, Ok(content) if *content == target_payload) {
            return Err(format!("{}. The file did not fill afterwards", trace.join("; ")));
        }
    } else {
        checks.note(ctx.fs, "read-only mount", &trace.join("; "));
        return Err(format!(
            "{}. An open through a read-only mount ended the helper (log: {})",
            trace.join("; "),
            tail(&log)
        ));
    }
    if r_result.is_ok() {
        return Err(format!(
            "{}. The kernel let an opener through onto a placeholder it could not hand over",
            trace.join("; ")
        ));
    }
    if r_after > Duration::from_secs(2) {
        return Err(format!("{}. The opener was left waiting", trace.join("; ")));
    }
    if !matches!(&b_result, Ok(content) if *content == slow_payload) {
        return Err(format!("{}. The suspended reader did not get its file", trace.join("; ")));
    }
    Ok(())
}

/// N1, second trigger. A user's own executable in the sync folder carries
/// no konedrive xattrs, so it is never ignore-marked (H5) and every open of
/// it is intercepted; while it runs, `deny_write_access` makes the kernel's
/// `O_RDWR` open of the next event's descriptor fail `ETXTBSY`. The helper
/// exited on that too, and the reader waiting for a slow hydration got zeros.
fn running_executable_open_survived(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let slow_payload: Vec<u8> = (0..65536usize).map(|i| (i % 187) as u8 + 1).collect();
    let slow = ctx.place("exec/slow.bin", "ITEM_EXEC_SLOW", &slow_payload)?;
    let pid = ctx.helper_pid();
    let dir_ino = ctx.ino_of(&ctx.root.join("exec"))?;
    let exe = ctx.root.join("exec/sleeper");
    let _ = std::fs::remove_file(&exe);
    std::fs::copy("/usr/bin/sleep", &exe).map_err(|e| format!("cannot copy sleep: {e}"))?;
    let log = ctx.helper.lock().unwrap().log.clone();
    let refused_before = occurrences_in_log(&log, EVENT_FD_FAILED);

    // The exec's own open is intercepted like any other, and the helper
    // closes its event descriptor a moment after answering; an exec that
    // gets there first fails `ETXTBSY`, so it is simply tried again.
    let mut attempts = 0;
    let mut runner = loop {
        attempts += 1;
        match Command::new(&exe)
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 10 => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("cannot run the copy of sleep: {e}")),
        }
    };
    std::thread::sleep(Duration::from_millis(300));
    let result = (|| -> Result<(), String> {
        if !matches!(runner.try_wait(), Ok(None)) {
            return Err("the copy of sleep is not running".into());
        }
        ctx.set_source_delay(Duration::from_millis(4000));
        let before = ctx.fetches();
        let b = Reader::start(&ctx.exe, &slow)?;
        ctx.wait_for_fetch(before, Duration::from_secs(10))?;

        let r = Reader::start(&ctx.exe, &exe)?;
        let (r_result, r_after) = r.get_timed(Duration::from_secs(10))?;
        std::thread::sleep(Duration::from_millis(300));
        let alive = ctx.helper_alive();
        let group_kept = alive && dir_mark_present(pid, dir_ino);
        // A second exec is an open of the same file too; measured, not
        // asserted — what it gets is the kernel's answer, not the helper's.
        let second_exec = if alive {
            match Command::new(&exe).arg("0").stdin(Stdio::null()).status() {
                Ok(status) => format!("ran ({status})"),
                Err(e) => format!("failed: {e}"),
            }
        } else {
            "not tried".into()
        };
        let b_result = b.get(Duration::from_secs(20))?;
        ctx.set_source_delay(Duration::from_millis(0));
        let refused =
            if alive { settled_count(&log, EVENT_FD_FAILED, refused_before) } else { 0 };
        let trace = format!(
            "running after {attempts} exec attempt(s); a second opener of it got {}; a second \
             exec of it {second_exec}; helper alive: {alive}, its directory mark {}; the helper \
             logged {refused} event(s) it could not be handed; the reader suspended on slow.bin \
             got {}",
            answered(&r_result, r_after),
            present(group_kept),
            got(&b_result, &slow_payload),
        );
        checks.note(ctx.fs, "running executable", &trace);
        if !alive {
            return Err(format!(
                "{trace}. A second open of a running executable ended the helper (log: {})",
                tail(&log)
            ));
        }
        if r_after > Duration::from_secs(2) {
            return Err(format!("{trace}. The opener was left waiting"));
        }
        if !matches!(&b_result, Ok(content) if *content == slow_payload) {
            return Err(format!("{trace}. The suspended reader did not get its file"));
        }
        Ok(())
    })();
    let _ = runner.kill();
    let _ = runner.wait();
    ctx.set_source_delay(Duration::from_millis(0));
    let _ = std::fs::remove_file(&exe);
    result
}

/// A hydrated, ignore-marked file for each of `names`, in a folder that is
/// then registered **without** interception, each still carrying its mark —
/// made the race-free way `carried_in_ignore_mark` makes one: hydrated in the
/// folder while it was intercepted, moved out, the folder forgotten (whose
/// walk therefore never met it), moved back in. Returns each path, inode and
/// payload.
fn stale_marked_files(
    ctx: &Ctx,
    service: &SyncService,
    folder: &Path,
    aside: &Path,
    names: &[&str],
) -> Result<Vec<(PathBuf, u64, Vec<u8>)>, String> {
    let pid = ctx.helper_pid();
    ctx.runtime
        .block_on(service.register_root(folder))
        .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    let mut files = Vec::new();
    for name in names {
        let item = format!("ITEM_STALE_{}_{name}", folder.file_name().unwrap().to_string_lossy());
        files.push(hydrated_through_open(ctx, folder, name, &item)?);
    }
    for (path, _, _) in &files {
        std::fs::rename(path, aside.join(path.file_name().unwrap())).map_err(|e| e.to_string())?;
    }
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    for (path, _, _) in &files {
        std::fs::rename(aside.join(path.file_name().unwrap()), path).map_err(|e| e.to_string())?;
    }
    ctx.runtime
        .block_on(service.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register without interception: {e}"))?;
    for (path, ino, _) in &files {
        if !ignore_mark_present(pid, *ino) {
            return Err(format!("{} lost its ignore mark on the way", path.display()));
        }
    }
    Ok(files)
}

/// N2, and the pattern behind it. A
/// dehydration in a folder registered without interception used to send no
/// `ClearIgnore`, on the strength of a chain of reasoning: nothing there is
/// intercepted, and interception resumes only through a registration walk
/// that clears every file's mark. The review broke the chain a third time:
/// a stale-marked, emptied file moved from a subdirectory the walk has not
/// reached into one it has passed is never cleared, and its reader got
/// 65 536 zero bytes after no fetch.
///
/// The chain is gone. Every punch asks the helper to clear the file's mark
/// when there is a link, so no emptied file carries one, and the rename
/// during the walk has nothing left to carry.
fn rename_during_registration_walk(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "rename-walk")?;
    let aside = scenario_folder(ctx, "rename-walk-aside")?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let pid = ctx.helper_pid();
    let mut trace: Vec<String> = Vec::new();
    let result = (|| -> Result<(), String> {
        ctx.runtime
            .block_on(service.register_root(&folder))
            .map_err(|e| format!("register: {e}"))?;
        let big = folder.join("big");
        std::fs::create_dir(&big).map_err(|e| e.to_string())?;
        for i in 0..60000 {
            File::create(big.join(format!("f{i:05}"))).map_err(|e| e.to_string())?;
        }
        let big_dir = File::open(&big).map_err(|e| e.to_string())?;
        ctx.runtime.block_on(link.mark_dir(&big_dir)).map_err(|e| format!("MarkDir: {e}"))?;
        drop(big_dir);
        let (path, ino, payload) = hydrated_through_open(ctx, &big, "zz-last.bin", "ITEM_RENAME")?;
        let away = aside.join("zz-last.bin");
        std::fs::rename(&path, &away).map_err(|e| e.to_string())?;
        ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
        std::fs::rename(&away, &path).map_err(|e| e.to_string())?;
        ctx.runtime
            .block_on(service.register_root_without_interception(&folder))
            .map_err(|e| e.to_string())?;
        ctx.runtime.block_on(service.dehydrate(&path)).map_err(|e| format!("Dehydrate: {e}"))?;
        let marked_after_punch = ignore_mark_present(pid, ino);
        trace.push(format!(
            "freed up without interception, with the helper connected: {} bytes allocated, state \
             {:?}, ignore mark {}",
            ctx.blocks_of(&path)? * 512,
            ctx.state_of(&path)?,
            present(marked_after_punch)
        ));
        ctx.runtime.block_on(service.unregister_root()).map_err(|e| e.to_string())?;

        // While the registration walk works through big/, move the file up
        // into the root, whose files the walk has already cleared.
        let root_ino = ctx.ino_of(&folder)?;
        let from = path.clone();
        let to = folder.join("zz-last.bin");
        let mover = std::thread::spawn(move || -> String {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !dir_mark_present(pid, root_ino) {
                if Instant::now() > deadline {
                    return "the root was never marked".into();
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            std::thread::sleep(Duration::from_millis(5));
            match std::fs::rename(&from, &to) {
                Ok(()) => "moved".into(),
                Err(e) => format!("rename failed: {e}"),
            }
        });
        let started = Instant::now();
        ctx.runtime
            .block_on(service.register_root(&folder))
            .map_err(|e| format!("re-register: {e}"))?;
        let walk = started.elapsed();
        let moved = mover.join().unwrap_or_else(|_| "the mover panicked".into());
        trace.push(format!(
            "registration walk {walk:?}; mover: {moved}; ignore mark after the walk {}",
            present(ignore_mark_present(pid, ino))
        ));
        let now_at = folder.join("zz-last.bin");
        let f0 = ctx.fetches();
        let c = Reader::start(&ctx.exe, &now_at)?.get(Duration::from_secs(60))?;
        trace.push(format!("a reader got {} after {} fetch(es)", got(&c, &payload), ctx.fetches() - f0));
        checks.note(ctx.fs, "rename during the walk", &trace.join("; "));
        if !matches!(&c, Ok(content) if *content == payload) {
            return Err(format!("{}. The reader did not get the file", trace.join("; ")));
        }
        if marked_after_punch {
            return Err(format!(
                "{}. A file was emptied with the helper's ignore mark still on it",
                trace.join("; ")
            ));
        }
        Ok(())
    })();
    service.set_link(Some(link.clone()));
    if service.root().is_some() {
        let _ = ctx.runtime.block_on(service.unregister_root());
    }
    drop(service);
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&aside);
    result
}

/// local rule, at each of the three places that can empty a
/// file in a folder registered without interception — a dehydration,
/// startup recovery of an interrupted file, and the roll-back of a fill that
/// failed — each against a file that carries a stale ignore mark:
///
/// - with a helper link, the helper is asked to clear the mark first, and
///   nothing is emptied with a mark on it;
/// - with no link while a helper is running, nothing is emptied at all: the
///   daemon cannot clear a mark that group may hold, so it refuses and
///   retries later;
/// - with no helper running at all, no group of ours exists and no mark can:
///   the punch goes ahead.
///
/// Before the rule, the first two emptied the file under its mark — the
/// state the old chain called harmless until the folder is intercepted
/// again, and which N2 showed is not.
fn punch_without_interception_rule(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "local-rule")?;
    let aside = scenario_folder(ctx, "local-rule-aside")?;
    let empty_source = scenario_folder(ctx, "local-rule-source")?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let unlinked = SyncService::new(None, None, None);
    let result = local_rule_steps(ctx, checks, &service, &unlinked, &folder, &aside, &empty_source);

    for held in [&unlinked, &service] {
        if held.root().is_some() {
            let _ = ctx.runtime.block_on(held.unregister_root());
        }
    }
    drop(service);
    drop(unlinked);
    if let Ok(link) = ctx.link() {
        release_at_the_helper(ctx, &link, &folder);
    }
    for dir in [&folder, &aside, &empty_source] {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

fn local_rule_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    service: &SyncService,
    unlinked: &SyncService,
    folder: &Path,
    aside: &Path,
    empty_source: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let mut trace: Vec<String> = Vec::new();
    let mut wrong: Vec<String> = Vec::new();
    let files = stale_marked_files(ctx, service, folder, aside, &["a.bin", "b.bin", "c.bin", "d.bin"])?;
    let (a, a_ino, _) = &files[0];
    let (b, b_ino, _) = &files[1];
    let (c, c_ino, _) = &files[2];
    let (d, d_ino, d_payload) = &files[3];
    trace.push("four hydrated files carrying stale ignore marks, folder without interception".into());

    // A dehydration, with a link.
    let freed = ctx.runtime.block_on(service.dehydrate(a));
    let a_marked = ignore_mark_present(pid, *a_ino);
    trace.push(format!(
        "Dehydrate with a link → {}: state {:?}, ignore mark {}",
        outcome(&freed),
        ctx.state_of(a)?,
        present(a_marked)
    ));
    if freed.is_err() || a_marked {
        wrong.push("a dehydration with a link emptied a file under its ignore mark, or failed".into());
    }

    // Recovery of a file a crash left `dehydrating` before its
    // `ClearIgnore`, with a link: brought up again, the folder recovers it.
    {
        let file = File::options().read(true).write(true).open(b).map_err(|e| e.to_string())?;
        write_state(&file, State::Dehydrating).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    ctx.runtime
        .block_on(service.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register without interception: {e}"))?;
    let b_marked = ignore_mark_present(pid, *b_ino);
    trace.push(format!(
        "recovery with a link: state {:?}, {} bytes allocated, ignore mark {}",
        ctx.state_of(b)?,
        ctx.blocks_of(b)? * 512,
        present(b_marked)
    ));
    if ctx.state_of(b)? != Some(State::OnlineOnly) || b_marked {
        wrong.push("recovery with a link did not reset the file, or emptied it under its mark".into());
    }

    // A fill that fails and rolls back: `Hydrate()` of a file labelled
    // `hydrated` with no stamp (H109), from a source that has nothing.
    xattr::remove(c, "user.konedrive.stamp").map_err(|e| e.to_string())?;
    ctx.runtime
        .block_on(service.populate_from_directory(empty_source))
        .map_err(|e| format!("cannot set an empty source: {e}"))?;
    let filled = ctx.runtime.block_on(service.hydrate_now(c));
    let c_marked = ignore_mark_present(pid, *c_ino);
    trace.push(format!(
        "a failing Hydrate with a link → {}: state {:?}, {} bytes allocated, ignore mark {}",
        outcome(&filled),
        ctx.state_of(c)?,
        ctx.blocks_of(c)? * 512,
        present(c_marked)
    ));
    if c_marked && ctx.holds_no_data(c).is_ok() {
        wrong.push("a failed fill emptied a file under its ignore mark".into());
    }

    // No link, while a helper is running: refused, and nothing emptied.
    ctx.runtime.block_on(service.unregister_root()).map_err(|e| format!("Forget: {e}"))?;
    ctx.runtime
        .block_on(unlinked.register_root_without_interception(folder))
        .map_err(|e| format!("cannot register without interception, unlinked: {e}"))?;
    let refused = ctx.runtime.block_on(unlinked.dehydrate(d));
    let d_marked = ignore_mark_present(pid, *d_ino);
    trace.push(format!(
        "Dehydrate with no link, a helper running → {}: state {:?}, {} bytes allocated, ignore \
         mark {}",
        outcome(&refused),
        ctx.state_of(d)?,
        ctx.blocks_of(d)? * 512,
        present(d_marked)
    ));
    if !matches!(refused, Err(SyncError::NoHelper)) || ctx.holds_no_data(d).is_ok() {
        wrong.push(
            "with a helper running and no link to it, a dehydration must be refused NoHelper and \
             empty nothing"
                .into(),
        );
    }

    // No helper at all: the group is gone with its marks, and the punch goes
    // ahead. Killed, not stopped, so the socket file stays behind with
    // nothing bound to it.
    {
        let mut helper = ctx.helper.lock().unwrap();
        let _ = helper.child.kill();
        let _ = helper.child.wait();
    }
    let stale_file = Path::new(SOCKET_PATH).exists();
    let freed = ctx.runtime.block_on(unlinked.dehydrate(d));
    trace.push(format!(
        "Dehydrate with no helper running (socket file left behind: {stale_file}) → {}: state {:?}",
        outcome(&freed),
        ctx.state_of(d)?
    ));
    ctx.restart_helper()?;
    if freed.is_err() {
        wrong.push("with no helper running, a dehydration must go ahead".into());
    }
    let _ = d_payload;
    checks.note(ctx.fs, "the local rule", &trace.join("; "));
    if !wrong.is_empty() {
        return Err(format!("{}. {}", trace.join("; "), wrong.join("; ")));
    }
    Ok(())
}

/// other half, at the helper. `ClearIgnore` used to be allowed
/// only on the filesystem of one of the asking uid's registered roots, so a
/// daemon whose folder is registered without interception could not ask at
/// all where it holds no helper root. Removing an ignore mark can only cause
/// one extra interception, never zeros, so the helper now decides it by who
/// owns the file — and still refuses a file the asker does not own, and
/// anything that is not a regular file.
fn clear_ignore_by_ownership(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let scratch = ScratchFs::create(ctx.fs, "owned")?;
    let link = ctx.link()?;
    let result = (|| -> Result<(), String> {
        let mine = scratch.mount.join("mine.bin");
        std::fs::write(&mine, [1u8; 4096]).map_err(|e| e.to_string())?;
        let theirs = scratch.mount.join("theirs.bin");
        std::fs::write(&theirs, [2u8; 4096]).map_err(|e| e.to_string())?;
        std::os::unix::fs::chown(&theirs, Some(HOSTILE_UID), Some(HOSTILE_UID))
            .map_err(|e| e.to_string())?;
        let dir = scratch.mount.join("dir");
        std::fs::create_dir(&dir).map_err(|e| e.to_string())?;

        let ask = |path: &Path| -> Result<String, String> {
            let file = File::open(path).map_err(|e| e.to_string())?;
            Ok(match ctx.runtime.block_on(link.clear_ignore(&file)) {
                Ok(()) => "Ok".into(),
                Err(e) => format!("{e}"),
            })
        };
        let on_mine = ask(&mine)?;
        let on_theirs = ask(&theirs)?;
        let on_dir = ask(&dir)?;
        let trace = format!(
            "on a filesystem where this uid holds no root: ClearIgnore on its own file → \
             {on_mine}; on another uid's file → {on_theirs}; on its own directory → {on_dir}"
        );
        checks.note(ctx.fs, "ClearIgnore by ownership", &trace);
        if on_mine != "Ok" {
            return Err(format!("{trace}. A uid must be able to clear the mark of its own file"));
        }
        if on_theirs == "Ok" || on_dir == "Ok" {
            return Err(format!(
                "{trace}. Only a regular file the asking uid owns may have its mark cleared"
            ));
        }
        Ok(())
    })();
    drop(scratch);
    result
}

/// N3. After a reconnect the previous
/// connection's fills keep running while the new connection's
/// recovery walks, and recovery took no per-inode lock and did not read the
/// state again under its lease. A fill that committed `hydrated` between
/// recovery's `ClearIgnore` and its lease — with an opener having the file
/// ignore-marked in that gap — was punched with the mark on it: the next
/// reader got 65 536 zero bytes after no fetch. The natural window is under
/// a millisecond; the suite's build of the daemon stalls recovery there
/// (`root::fault`, compiled only under `fault-injection`).
///
/// The "old fill" runs in this process — the exempt daemon — and is run
/// twice: holding the per-inode lock, as every fill of the daemon's does,
/// and without it, as one whose descriptor's identity could not be read
/// does. Recovery is the real one: `SyncService::resume`, as a reconnect
/// runs it.
fn recovery_overtaken_by_old_fill(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "recovery-race")?;
    let link = ctx.link()?;
    let service = SyncService::new(Some(link.clone()), None, None);
    let mut trace: Vec<String> = Vec::new();
    let result = (|| -> Result<(), String> {
        ctx.runtime
            .block_on(service.register_root(&folder))
            .map_err(|e| format!("register: {e}"))?;
        let mut wrong = Vec::new();
        for (name, locked) in [("locked.bin", true), ("unlocked.bin", false)] {
            let outcome = old_fill_against_recovery(ctx, &service, &folder, name, locked)?;
            if !outcome.1 {
                wrong.push(name);
            }
            trace.push(outcome.0);
        }
        checks.note(ctx.fs, "recovery and an old fill", &trace.join(" | "));
        if !wrong.is_empty() {
            return Err(format!(
                "{}. A reader did not get the file after recovery met a fill that committed \
                 meanwhile ({wrong:?})",
                trace.join(" | ")
            ));
        }
        Ok(())
    })();
    konedrived::sync::root::fault::set_recovery_stall(Duration::ZERO);
    if service.root().is_some() {
        let _ = ctx.runtime.block_on(service.unregister_root());
    }
    drop(service);
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    result
}

/// One run of [`recovery_overtaken_by_old_fill`]: the trace, and whether
/// every reader got the file.
fn old_fill_against_recovery(
    ctx: &Ctx,
    service: &Arc<SyncService>,
    folder: &Path,
    name: &str,
    locked: bool,
) -> Result<(String, bool), String> {
    use std::os::unix::fs::FileExt;

    let pid = ctx.helper_pid();
    let payload: Vec<u8> = (0..65536usize).map(|i| (i % 211) as u8 + 1).collect();
    let item = format!("ITEM_RECRACE_{name}");
    std::fs::write(ctx.source_dir.join(&item), &payload).map_err(|e| e.to_string())?;
    let dir = File::open(folder).map_err(|e| e.to_string())?;
    create_placeholder(
        &dir,
        name,
        &item,
        payload.len() as u64,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    )
    .map_err(|e| e.to_string())?;
    drop(dir);
    let x = folder.join(name);
    let ino = ctx.ino_of(&x)?;

    // The old connection's fill, in progress: `hydrating`, content going in.
    let fill = File::options().read(true).write(true).open(&x).map_err(|e| e.to_string())?;
    let key = InodeKey::of(&fill).map_err(|e| e.to_string())?;
    let guard = if locked { Some(ctx.runtime.block_on(service.locks().lock(key))) } else { None };
    write_state(&fill, State::Hydrating).map_err(|e| e.to_string())?;
    fill.sync_all().map_err(|e| e.to_string())?;
    fill.write_all_at(&payload, 0).map_err(|e| e.to_string())?;

    // The reconnect: re-registration, then recovery, stalled between its
    // `ClearIgnore` and its lease.
    konedrived::sync::root::fault::set_recovery_stall(Duration::from_millis(2000));
    let resuming = Arc::clone(service);
    let recovery = ctx.runtime.spawn(async move { resuming.resume().await });
    std::thread::sleep(Duration::from_millis(700));

    // The old fill commits and lets go; an opener finds the file hydrated,
    // and the helper ignore-marks it.
    konedrive_fs::placeholder::write_stamp(&fill).map_err(|e| e.to_string())?;
    write_state(&fill, State::Hydrated).map_err(|e| e.to_string())?;
    fill.sync_all().map_err(|e| e.to_string())?;
    drop(fill);
    drop(guard);
    let b = ctx.read(&x)?;
    let b_marked = ignore_mark_present(pid, ino);
    ctx.runtime.block_on(recovery).map_err(|e| format!("resume panicked: {e}"))?;
    konedrived::sync::root::fault::set_recovery_stall(Duration::ZERO);

    let after = ctx.state_of(&x)?;
    let allocated = ctx.blocks_of(&x)? * 512;
    let marked = ignore_mark_present(pid, ino);
    let f0 = ctx.fetches();
    let c = Reader::start(&ctx.exe, &x)?.get(Duration::from_secs(60))?;
    let fetched = ctx.fetches() - f0;
    let fine = matches!(&c, Ok(content) if *content == payload) && b == payload;
    Ok((
        format!(
            "{} the per-inode lock: the fill committed while recovery ran; reader B got {}, \
             ignore mark {}; after recovery ({:?}, {:?}): state {after:?}, {allocated} bytes \
             allocated, ignore mark {}; reader C got {} after {fetched} fetch(es)",
            if locked { "holding" } else { "without" },
            got(&Ok(b.clone()), &payload),
            present(b_marked),
            service.root_state(),
            service.last_error(),
            present(marked),
            got(&c, &payload),
        ),
        fine,
    ))
}

fn present(yes: bool) -> &'static str {
    if yes {
        "present"
    } else {
        "absent"
    }
}

fn drop_caches() -> Result<(), String> {
    // SAFETY: a plain sync(2).
    unsafe { libc::sync() };
    std::fs::write("/proc/sys/vm/drop_caches", b"3")
        .map_err(|e| format!("cannot drop caches: {e}"))
}

fn new_directory_covered(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("created-later");
    let _ = std::fs::remove_dir_all(&dir);
    // Created outside the daemon, the way a user's `mkdir` would.
    Command::new("mkdir")
        .arg(&dir)
        .status()
        .map_err(|e| format!("cannot run mkdir: {e}"))?;
    // The daemon has no live directory watcher today: it marks a directory
    // when it creates one (`populate_walk`, invariant M1) and the helper
    // covers whatever exists at startup. A directory somebody else created
    // stays uncovered until one of those two happens, which is what this
    // records.
    let ino = ctx.ino_of(&dir)?;
    if dir_mark_present(ctx.helper_pid(), ino) {
        checks.note(ctx.fs, "new directory", "was covered without anyone asking");
    } else {
        checks.note(
            ctx.fs,
            "new directory",
            "a directory created behind the daemon's back is NOT covered until the daemon marks \
             it or the helper restarts — there is no live watcher",
        );
    }

    let dirfile = File::open(&dir).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    ctx.runtime
        .block_on(link.mark_dir(&dirfile))
        .map_err(|e| format!("MarkDir on a new directory failed: {e}"))?;
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("MarkDir acknowledged success but fdinfo shows no mark".into());
    }

    let path = ctx.place("created-later/inside.bin", "ITEM_NEWDIR", b"INSIDE")?;
    let before = ctx.fetches();
    let content = ctx.read(&path)?;
    if content != b"INSIDE" {
        return Err(format!("the reader got {content:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

fn moved_out_still_covered(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // The control first: a placeholder moved out of the tree with nothing
    // done about it. §1 of the kernel notes expects this to escape the
    // parent's mark entirely, and that is exactly why `MarkFile` exists.
    let loose = ctx.place("loose.bin", "ITEM_LOOSE", b"LOOSE")?;
    let moved_loose = ctx.outside.join("loose.bin");
    let _ = std::fs::remove_file(&moved_loose);
    std::fs::rename(&loose, &moved_loose).map_err(|e| e.to_string())?;
    let outcome = ctx.read(&moved_loose);
    match &outcome {
        Ok(content) if content.iter().all(|b| *b == 0) => checks.note(
            ctx.fs,
            "rename out of the tree",
            "an unmarked file that left the root is NOT intercepted: the open succeeded and read \
             zeros, which is what invariant M4's individual mark exists to prevent",
        ),
        Ok(content) => checks.note(
            ctx.fs,
            "rename out of the tree",
            &format!("the open returned {} bytes of real content", content.len()),
        ),
        Err(e) => checks.note(
            ctx.fs,
            "rename out of the tree",
            &format!("the open was still intercepted: {e}"),
        ),
    }

    // Now the case the design promises: mark the file individually before it
    // leaves, and it stays covered wherever it goes.
    let path = ctx.place("leaving.bin", "ITEM_MOVED", b"MOVED")?;
    let file = File::options().read(true).write(true).open(&path).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    ctx.runtime
        .block_on(link.mark_file(&file))
        .map_err(|e| format!("MarkFile failed: {e}"))?;
    let ino = ctx.ino_of(&path)?;
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("MarkFile acknowledged success but fdinfo shows no mark on the file".into());
    }
    drop(file);

    let moved = ctx.outside.join("moved.bin");
    let _ = std::fs::remove_file(&moved);
    std::fs::rename(&path, &moved).map_err(|e| e.to_string())?;

    let before = ctx.fetches();
    let content = ctx.read(&moved)?;
    if content != b"MOVED" {
        return Err(format!("the reader got {content:?} after the move"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

/// Review item 10's remaining two holes, measured rather than assumed. Neither
/// has an assertion attached: what the kernel does here is a fact to record,
/// and the design's own answer to both is invariant M4's individual mark,
/// which the scenario above already proves works.
fn hardlink_and_second_mount(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // A hardlink to a placeholder, in a directory nobody marked.
    let path = ctx.place("linked.bin", "ITEM_LINK", b"LINKED")?;
    let link_path = ctx.outside.join("hardlink.bin");
    let _ = std::fs::remove_file(&link_path);
    std::fs::hard_link(&path, &link_path).map_err(|e| format!("cannot hardlink: {e}"))?;
    let before = ctx.fetches();
    match ctx.read(&link_path) {
        Ok(content) if content == b"LINKED" => checks.note(
            ctx.fs,
            "hardlink in an unmarked directory",
            &format!(
                "the open WAS intercepted and filled ({} fetch(es))",
                ctx.fetches() - before
            ),
        ),
        Ok(content) if content.iter().all(|b| *b == 0) => checks.note(
            ctx.fs,
            "hardlink in an unmarked directory",
            "the open was NOT intercepted: it read zeros, confirming that a parent's mark is \
             consulted during resolution through that parent and is not a property of the inode",
        ),
        Ok(content) => checks.note(
            ctx.fs,
            "hardlink in an unmarked directory",
            &format!("the open returned {} unexpected bytes", content.len()),
        ),
        Err(e) => {
            checks.note(
                ctx.fs,
                "hardlink in an unmarked directory",
                &format!("the open failed: {e}"),
            )
        }
    }
    // Whatever happened, leave the inode in a known state for the next
    // scenario: through the marked name it is either hydrated or not.
    let _ = std::fs::remove_file(&link_path);

    // A second mount of the same filesystem. A bind mount shares the
    // superblock and therefore the inodes, so if a mark were per-mount this
    // is where it would show.
    let second = PathBuf::from(format!("/mnt/{}-second", ctx.fs));
    let _ = std::fs::create_dir_all(&second);
    let status = Command::new("mount")
        .arg("--bind")
        .arg(format!("/mnt/{}", ctx.fs))
        .arg(&second)
        .status()
        .map_err(|e| format!("cannot bind-mount: {e}"))?;
    if !status.success() {
        checks.note(ctx.fs, "second mount", "the bind mount failed; not measured");
        return Ok(());
    }
    let via_second = second.join(
        ctx.root
            .strip_prefix(format!("/mnt/{}", ctx.fs))
            .map_err(|e| e.to_string())?,
    );
    let other = ctx.place("second-mount.bin", "ITEM_SECOND", b"SECOND")?;
    let _ = other;
    let through = via_second.join("second-mount.bin");
    let before = ctx.fetches();
    let outcome = ctx.read(&through);
    match &outcome {
        Ok(content) if content == b"SECOND" => checks.note(
            ctx.fs,
            "second mount",
            &format!(
                "an open through a second mount of the same filesystem IS intercepted and filled \
                 ({} fetch(es))",
                ctx.fetches() - before
            ),
        ),
        Ok(content) if content.iter().all(|b| *b == 0) => checks.note(
            ctx.fs,
            "second mount",
            "an open through a second mount is NOT intercepted: it read zeros",
        ),
        Ok(content) => checks.note(
            ctx.fs,
            "second mount",
            &format!("the open returned {} unexpected bytes", content.len()),
        ),
        Err(e) => checks.note(ctx.fs, "second mount", &format!("the open failed: {e}")),
    }
    let _ = Command::new("umount").arg(&second).status();
    Ok(())
}

fn zero_byte_file(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("empty.bin", "ITEM_EMPTY", b"")?;
    let before = ctx.fetches();
    let started = Instant::now();
    let content = ctx.read(&path)?;
    let elapsed = started.elapsed();
    if !content.is_empty() {
        return Err(format!("a zero-byte file returned {} bytes", content.len()));
    }
    if ctx.fetches() != before {
        return Err(format!("a zero-byte file caused {} fetch(es)", ctx.fetches() - before));
    }
    match ctx.state_of(&path)? {
        Some(State::Hydrated) => {}
        other => return Err(format!("a zero-byte placeholder is {other:?}, not hydrated")),
    }
    if elapsed > Duration::from_secs(5) {
        return Err(format!("the open took {elapsed:?}, so it did not go straight through"));
    }
    Ok(())
}

/// Review item 5. `FAN_DENY | (errno << 24)` is accepted by the kernel for
/// eight values and refused for every other, and a refused response leaves the
/// opener suspended **forever** (§5). The daemon reports errnos the helper does
/// not choose, so every value it could ever produce is swept here against a
/// live suspended opener, and the property asserted is the one that matters:
/// nobody is left unanswered.
fn errno_sweep(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("sweep.bin", "ITEM_SWEEP", b"SWEPT")?;
    let mut accepted: Vec<i32> = Vec::new();
    let mut delivered: BTreeMap<i32, i32> = BTreeMap::new();
    let mut unanswered: Vec<i32> = Vec::new();

    let mut candidates: Vec<i32> = (0..=133).collect();
    // Values a daemon has no business sending, and which must not corrupt the
    // response word either.
    candidates.extend([256, 512, 4095, -1, i32::MAX, i32::MIN]);

    let responder = Responder::start().map_err(|e| format!("cannot start the responder: {e}"))?;
    for errno in candidates {
        responder.answer_with(errno);
        // A fresh placeholder every time: a file the helper has already
        // allowed carries an ignore mark and raises no event at all.
        let name = format!("sweep-{}.bin", errno as i64 & 0xffff_ffff);
        let path = ctx
            .place(&name, "ITEM_SWEEP", b"SWEPT")
            .map_err(|e| format!("cannot place a sweep placeholder: {e}"))?;
        let reader = Reader::start(&ctx.exe, &path)?;
        match reader.get(Duration::from_secs(15)) {
            Ok(Ok(_)) => {
                accepted.push(errno);
            }
            Ok(Err(got)) => {
                delivered.insert(errno, got);
            }
            Err(_) => {
                unanswered.push(errno);
                // A suspended opener that is never answered holds an event fd
                // in the helper for the rest of the run; killing it does not
                // release the kernel's side, so the rest of the sweep is not
                // worth continuing.
                reader.kill();
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    drop(responder);
    let _ = path;
    // Hydrations go back to the daemon underneath, which the
    // rest of the suite needs.
    let back = ctx.place("sweep-after.bin", "ITEM_SWEEP_AFTER", b"THE DAEMON AGAIN")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Reader::start(&ctx.exe, &back)?.get(Duration::from_secs(60))? {
            Ok(content) if content == b"THE DAEMON AGAIN" => break,
            other if Instant::now() > deadline => {
                return Err(format!("after the sweep, the daemon did not get hydrations back: {other:?}"))
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }

    checks.note(
        ctx.fs,
        "errno sweep",
        &format!(
            "{} value(s) let the open through, {} were delivered as an errno, {} left the opener \
             hanging",
            accepted.len(),
            delivered.len(),
            unanswered.len()
        ),
    );
    // Which errnos arrive unchanged is the interesting part: everything else
    // is clamped to EIO by `clamp_deny_errno`, or refused by the kernel and
    // rescued with a plain FAN_DENY (EPERM).
    let verbatim: Vec<i32> =
        delivered.iter().filter(|(asked, got)| *asked == *got).map(|(asked, _)| *asked).collect();
    checks.note(ctx.fs, "errno sweep", &format!("delivered verbatim: {verbatim:?}"));
    let as_eperm: Vec<i32> =
        delivered.iter().filter(|(_, got)| **got == libc::EPERM).map(|(a, _)| *a).collect();
    if !as_eperm.is_empty() {
        checks.note(
            ctx.fs,
            "errno sweep",
            &format!("arrived as EPERM (a plain FAN_DENY rescue): {as_eperm:?}"),
        );
    }

    if !unanswered.is_empty() {
        return Err(format!(
            "these errnos left a suspended opener with no answer at all: {unanswered:?}"
        ));
    }
    Ok(())
}

/// Review item 6. The socket is 0666 by design, so everything that protects
/// one user's hydrations from another has to be an authorisation check inside
/// the helper. Three of them, driven from a process running as another uid.
fn hostile_uid(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("victim.bin", "ITEM_VICTIM", b"VICTIM")?;
    ctx.set_source_delay(Duration::from_secs(3));
    let before = ctx.fetches();
    let reader = Reader::start(&ctx.exe, &path)?;
    ctx.wait_for_fetch(before, Duration::from_secs(20))?;

    // The suite's own binary lives under the developer's home directory,
    // which another uid cannot even traverse; a copy on the guest's tmpfs is
    // what a stranger on this machine would actually have.
    let hostile_exe = Path::new("/run/konedrive-hostile-client");
    std::fs::copy(&ctx.exe, hostile_exe)
        .map_err(|e| format!("cannot stage the hostile client: {e}"))?;
    std::fs::set_permissions(
        hostile_exe,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .map_err(|e| e.to_string())?;
    let output = Command::new(hostile_exe)
        .arg("--hostile")
        .arg(&ctx.sync_root().root_id)
        .env("KONEDRIVE_VICTIM_ROOT", &ctx.root)
        .uid(HOSTILE_UID)
        .gid(HOSTILE_UID)
        .output()
        .map_err(|e| format!("cannot run the hostile client: {e}"))?;
    let said = String::from_utf8_lossy(&output.stdout).into_owned();
    for line in said.lines() {
        checks.note(ctx.fs, "hostile client", line);
    }

    // The victim's hydration must complete normally: the guessed
    // `HydrateDone`s must not have force-allowed it onto an unfilled file.
    let content = reader.content(Duration::from_secs(30))?;
    if content != b"VICTIM" {
        return Err(format!(
            "another uid changed the outcome of a hydration: the reader got {content:?}"
        ));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }

    if !said.contains("UNREGISTER errno=1") {
        return Err(format!("UnregisterRoot from another uid was not refused EPERM: {said:?}"));
    }
    if !said.contains("UNMARKDIR errno=1") {
        return Err(format!("UnmarkDir from another uid was not refused EPERM: {said:?}"));
    }

    // And the root is still registered and still marked.
    let root_ino = ctx.ino_of(&ctx.root)?;
    if !dir_mark_present(ctx.helper_pid(), root_ino) {
        return Err("another uid managed to take the mark off the root directory".into());
    }
    ctx.source.reset();
    let after = ctx.place("after-hostile.bin", "ITEM_AFTERHOSTILE", b"STILL WORKS")?;
    if ctx.read(&after)? != b"STILL WORKS" {
        return Err("interception stopped working after the hostile client ran".into());
    }
    Ok(())
}

/// Review item 7, first half. `SO_PEERCRED` on a `SOCK_SEQPACKET` socket must
/// report the pid the fanotify event reports, or the daemon's narrow exemption
/// either does not fire — deadlocking startup recovery — or fires
/// for the wrong process. It is observable from here precisely because the
/// exemption is: this process holds the connection, so its own open of an
/// `online-only` placeholder must go straight through with no fetch, while the
/// same open from any other process must be intercepted and filled.
///. Every connection costs the helper
/// two threads and about three descriptors, and the socket is 0666: any
/// local user could open connections until the helper ran out of
/// descriptors, and from then on every intercepted open on the machine was
/// denied. A uid gets a bounded number of live connections; beyond it a
/// connection is closed at once, and nobody else's is touched.
fn connections_per_uid_are_capped(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let hostile_exe = Path::new("/run/konedrive-hostile-client");
    std::fs::copy(&ctx.exe, hostile_exe)
        .map_err(|e| format!("cannot stage the hostile client: {e}"))?;
    std::fs::set_permissions(hostile_exe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let output = Command::new(hostile_exe)
        .arg("--connections")
        .arg("40")
        .uid(HOSTILE_UID)
        .gid(HOSTILE_UID)
        .output()
        .map_err(|e| format!("cannot run the hostile client: {e}"))?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    checks.note(ctx.fs, "connections per uid", &said);
    let greeted: usize = said
        .strip_prefix("GREETED ")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("the client said {said:?}"))?;
    let after = ctx.place("after-connections.bin", "ITEM_AFTER_CONNECTIONS", b"STILL SERVED")?;
    if ctx.read(&after)? != b"STILL SERVED" {
        return Err("interception stopped working after one uid's connections".into());
    }
    if greeted >= 40 {
        return Err(format!(
            "{said}: one uid holds every connection it asks for, so any local user can run the \
             helper out of descriptors"
        ));
    }
    Ok(())
}

fn peercred_pid_matches_event_pid(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("exempt.bin", "ITEM_EXEMPT", b"EXEMPT")?;
    let before = ctx.fetches();
    let mut content = Vec::new();
    File::open(&path)
        .map_err(|e| format!("the daemon's own open was not exempt: {e}"))?
        .read_to_end(&mut content)
        .map_err(|e| e.to_string())?;
    if ctx.fetches() != before {
        return Err(format!(
            "the daemon's own open was hydrated ({} fetch(es)), so it was not exempt",
            ctx.fetches() - before
        ));
    }
    if !content.iter().all(|b| *b == 0) {
        return Err("an exempt open of an online-only placeholder returned content".into());
    }
    match ctx.state_of(&path)? {
        Some(State::OnlineOnly) => {}
        other => return Err(format!("the exempt open changed the state to {other:?}")),
    }

    // The converse: another process is not exempt.
    let filled = ctx.read(&path)?;
    if filled != b"EXEMPT" {
        return Err(format!("a non-daemon open got {filled:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!(
            "a non-daemon open caused {} fetch(es)",
            ctx.fetches() - before
        ));
    }
    Ok(())
}

/// Review item 7, second half. The helper's unit runs with
/// `ProtectHome=read-only`, which is a mount namespace in which the tree the
/// sync root lives on is read-only *for the helper*. What has to keep working
/// is the write the **daemon** performs through the descriptor the helper
/// handed it, so the question is whose mount the event fd is opened against.
/// A read-only bind mount in a private namespace reproduces it without
/// systemd.
fn readonly_mount_event_fd(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // The helper is restarted inside a mount namespace of its own where the
    // whole filesystem under test is read-only.
    let script = format!(
        "mount --bind /mnt/{fs} /mnt/{fs} && mount -o remount,bind,ro /mnt/{fs} && exec \"$@\"",
        fs = ctx.fs
    );
    let binary = ctx.helper.lock().unwrap().binary.clone();
    let log = ctx.helper.lock().unwrap().log.clone();
    ctx.kill_daemon();
    ctx.helper.lock().unwrap().stop();

    let out = File::options().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new("unshare")
        .arg("--mount")
        .arg("--propagation")
        .arg("private")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .arg(&binary)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the helper under unshare: {e}"))?;
    {
        let mut helper = ctx.helper.lock().unwrap();
        helper.child = child;
        helper.await_socket()?;
    }
    ctx.connect_daemon()?;

    let payload = vec![0x5au8; 4096];
    let path = ctx.place("readonly-ns.bin", "ITEM_ROMOUNT", &payload)?;
    let before = ctx.fetches();
    let outcome = ctx.read(&path);
    let result = match &outcome {
        Ok(content) if *content == payload => {
            checks.note(
                ctx.fs,
                "read-only mount",
                "the event fd stayed writable: the kernel opens it against the *opener's* mount, \
                 so ProtectHome=read-only on the helper does not stop a hydration",
            );
            Ok(())
        }
        Ok(content) => Err(format!(
            "the open returned {} bytes, of which {} are wrong",
            content.len(),
            content.iter().zip(&payload).filter(|(a, b)| a != b).count()
        )),
        Err(e) => Err(format!(
            "a hydration failed while the helper's own view of the filesystem was read-only: {e} \
             (this is what ProtectHome=read-only would do in production)"
        )),
    };
    let _ = ctx.fetches() - before;

    // Back to an ordinary helper for everything after this.
    ctx.restart_helper()?;
    result
}

/// Review item 9. Two hazards for the startup walk, in one tree: a symlinked
/// subdirectory (the helper walks as root, so following one is how a user has
/// every directory on the machine marked), and directories being renamed under
/// the walk while it runs.
fn startup_walk_hazards(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let tree = ctx.root.join("walk");
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(tree.join("real/inner")).map_err(|e| e.to_string())?;
    let target = ctx.outside.join("symlink-target");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(target.join("below")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(tree.join("escape"));
    std::os::unix::fs::symlink(&target, tree.join("escape")).map_err(|e| e.to_string())?;
    // A symlink to a directory *inside* the root, too: following it would
    // double-mark and, worse, is the shape that turns a walk into a loop.
    let _ = std::fs::remove_file(tree.join("loop"));
    std::os::unix::fs::symlink(&tree, tree.join("loop")).map_err(|e| e.to_string())?;

    // A wide tree for the swap to happen in.
    for i in 0..200 {
        std::fs::create_dir_all(tree.join(format!("swap-{i}/child")))
            .map_err(|e| e.to_string())?;
    }

    let renamer = {
        let tree = tree.clone();
        std::thread::spawn(move || {
            // Swap directories underneath the walk for as long as it lasts.
            let deadline = Instant::now() + Duration::from_secs(6);
            let mut n = 0u32;
            while Instant::now() < deadline {
                let i = (n % 200) as usize;
                let from = tree.join(format!("swap-{i}"));
                let to = tree.join(format!("swapped-{i}"));
                let _ = std::fs::rename(&from, &to);
                let _ = std::fs::create_dir(&from);
                let _ = std::fs::create_dir(from.join("child"));
                let _ = std::fs::remove_dir_all(&to);
                n += 1;
            }
            n
        })
    };
    ctx.restart_helper()?;
    let swaps = renamer.join().map_err(|_| "the renaming thread panicked")?;
    checks.note(ctx.fs, "startup walk", &format!("{swaps} directory swaps during the walk"));

    if !ctx.helper_alive() {
        return Err("the helper did not survive a tree changing under its startup walk".into());
    }
    let target_ino = ctx.ino_of(&target)?;
    if dir_mark_present(ctx.helper_pid(), target_ino) {
        return Err(format!(
            "the walk followed a symlink out of the root: ino {target_ino} ({}) is marked",
            target.display()
        ));
    }
    let below_ino = ctx.ino_of(&target.join("below"))?;
    if dir_mark_present(ctx.helper_pid(), below_ino) {
        return Err("the walk descended through a symlink out of the root".into());
    }
    let inner_ino = ctx.ino_of(&tree.join("real/inner"))?;
    if !dir_mark_present(ctx.helper_pid(), inner_ino) {
        return Err("a real subdirectory of the root was not marked by the startup walk".into());
    }

    // And interception still works in the part of the tree that stood still.
    let path = ctx.place("walk/real/inner/after.bin", "ITEM_WALK", b"WALKED")?;
    if ctx.read(&path)? != b"WALKED" {
        return Err("a file under the walked tree was not hydrated".into());
    }
    let _ = std::fs::remove_dir_all(&tree);
    Ok(())
}

/// Review item 12, second half. `readdir`'s `d_type` is only a hint, and some
/// filesystems report `DT_UNKNOWN` for everything; the walk must settle the
/// question with `openat2(O_DIRECTORY)` rather than believe the hint. An ext4
/// image built without the `filetype` feature is a filesystem that really does
/// answer `DT_UNKNOWN`, rather than a constructed one.
fn dt_unknown_walk(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    if ctx.fs != "btrfs" {
        // The image is filesystem-independent; running it once is enough, and
        // it is run under the first suite so a failure is seen early.
        checks.note(ctx.fs, "DT_UNKNOWN", "measured once, under the btrfs suite");
        return Ok(());
    }
    let image = PathBuf::from("/mnt/img/nofiletype.img");
    let mount = PathBuf::from("/mnt/nofiletype");
    let _ = std::fs::create_dir_all(&mount);
    if !image.exists() {
        Command::new("truncate")
            .args(["-s", "64M"])
            .arg(&image)
            .status()
            .map_err(|e| e.to_string())?;
        let out = Command::new("mkfs.ext4")
            .args(["-q", "-F", "-O", "^filetype"])
            .arg(&image)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            checks.note(
                ctx.fs,
                "DT_UNKNOWN",
                &format!(
                    "mkfs.ext4 -O ^filetype is not available here: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            );
            return Ok(());
        }
    }
    let status = Command::new("mount")
        .args(["-o", "loop"])
        .arg(&image)
        .arg(&mount)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("cannot mount the ^filetype image".into());
    }

    let result = (|| -> Result<(), String> {
        let root = mount.join("root");
        std::fs::create_dir_all(root.join("a/b/c")).map_err(|e| e.to_string())?;
        std::fs::write(root.join("a/plain"), b"x").map_err(|e| e.to_string())?;
        // The hint really is DT_UNKNOWN here, or this measures nothing.
        let unknown = nix::dir::Dir::open(
            root.join("a").as_path(),
            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|e| e.to_string())?
        .iter()
        .flatten()
        .all(|e| e.file_type().is_none());
        if !unknown {
            return Err("the ^filetype image still reports a d_type; nothing is measured here"
                .into());
        }

        // Registered through the helper directly rather than through
        // `root::register_root`: the daemon refuses a folder that is not
        // empty, and the tree has to be there *before* the walk
        // runs or there is nothing to walk.
        let link = ctx.link()?;
        let root_id = "dt-unknown-root";
        let handle = File::open(&root).map_err(|e| e.to_string())?;
        ctx.runtime
            .block_on(link.register_root(&handle, root_id))
            .map_err(|e| format!("cannot register the ^filetype root: {e}"))?;
        let deep = std::fs::metadata(root.join("a/b/c")).map_err(|e| e.to_string())?.ino();
        let marked = dir_mark_present(ctx.helper_pid(), deep);

        // The unregistration walk meets files here only as names whose
        // `d_type` is unknown, so the one way it learns that `a/hydrated` is
        // a file is the `ENOTDIR` from its `O_DIRECTORY` open — and that is
        // where the file's ignore mark has to come off (small round 3, item
        // 1). A file hydrated through an intercepted open carries one.
        let payload = b"UNKNOWN TYPE".to_vec();
        std::fs::write(ctx.source_dir.join("ITEM_DTUNKNOWN"), &payload)
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(root.join("a/hydrated"));
        let parent = File::open(root.join("a")).map_err(|e| e.to_string())?;
        create_placeholder(
            &parent,
            "hydrated",
            "ITEM_DTUNKNOWN",
            payload.len() as u64,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .map_err(|e| format!("cannot create a placeholder on the ^filetype image: {e}"))?;
        drop(parent);
        let hydrated = root.join("a/hydrated");
        let filled = ctx.read(&hydrated);
        let file_ino = ctx.ino_of(&hydrated)?;
        let ignored_before = ignore_mark_present(ctx.helper_pid(), file_ino);

        ctx.runtime
            .block_on(link.unregister_root(root_id))
            .map_err(|e| format!("cannot unregister the ^filetype root: {e}"))?;
        let ignored_after = ignore_mark_present(ctx.helper_pid(), file_ino);
        if !marked {
            return Err(
                "a directory three levels down a DT_UNKNOWN filesystem was not marked".into()
            );
        }
        match filled {
            Ok(content) if content == payload => {}
            Ok(content) => return Err(format!("the file on the ^filetype image read {content:?}")),
            Err(e) => return Err(format!("the file on the ^filetype image did not fill: {e}")),
        }
        if !ignored_before {
            return Err("the hydrated file carries no ignore mark, so nothing is tested".into());
        }
        if ignored_after {
            return Err(format!(
                "ino {file_ino} kept its ignore mark through UnregisterRoot: on a DT_UNKNOWN \
                 filesystem the walk did not clear the ignore mark of a file it only met as \
                 ENOTDIR"
            ));
        }
        Ok(())
    })();

    let _ = Command::new("umount").arg(&mount).status();
    result
}

/// Review item 12, first half. Running out of descriptors is the one failure
/// the event loop survives on purpose: the helper exiting sets
/// every outstanding permission event to *allowed*, which is silent data loss,
/// while a denial is an errno the application can see.
fn emfile_survived(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // A limit low enough that a modest burst exhausts it, but high enough for
    // the socket, the group, the roots file and the pool to exist at all.
    ctx.restart_helper_with(Some(128), None)?;
    let log = ctx.helper.lock().unwrap().log.clone();
    let exhausted_before = count_in_log(&log, "out of file descriptors");
    let dir = ctx.root.join("emfile");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let payload = vec![0x11u8; 4096];
    let count = 400usize;
    for i in 0..count {
        ctx.place(&format!("emfile/burst-{i}"), &format!("ITEM_EMFILE_{i}"), &payload)?;
    }
    ctx.set_source_delay(Duration::from_millis(200));
    let report = run_burst(ctx, &dir, count, 0x11)?;
    checks.note(ctx.fs, "EMFILE", &format!("{report}"));
    if report.answered != count {
        return Err(format!("{} of {count} openers never returned", count - report.answered));
    }
    if !ctx.helper_alive() {
        return Err("the helper exited when it ran out of descriptors".into());
    }
    // Counted over the whole log, not read off its last dozen lines: the
    // refusals a burst produces bury the one throttled line that says the
    // limit was hit, and the tail then reported "never reached" for a run in
    // which the kernel had already denied opens it could not hand over.
    if count_in_log(&log, "out of file descriptors") == exhausted_before {
        checks.note(
            ctx.fs,
            "EMFILE",
            "the limit was never actually reached; the burst did not exhaust 128 descriptors",
        );
    }

    ctx.source.reset();
    ctx.restart_helper_with(None, None)?;
    let after = ctx.place("after-emfile.bin", "ITEM_AFTEREMFILE", b"AFTER")?;
    match ctx.read(&after) {
        Ok(content) if content == b"AFTER" => Ok(()),
        Ok(content) => Err(format!("the follow-up open returned {content:?}")),
        Err(e) => Err(format!(
            "interception did not come back after the descriptor shortage: {e}; the helper's \
             last words were: {}",
            ctx.helper_log_tail()
        )),
    }
}

/// Review item 11, first half. Nothing input-reachable panics in a worker any
/// more, so the unwind path that exists for has to be injected. The
/// helper is restarted with a fault armed on a distinctive file size.
fn worker_panic_contained(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    const MAGIC: usize = 57005;
    ctx.restart_helper_with(None, Some(("KONEDRIVE_FAULT_PANIC_ON_SIZE", "57005")))?;
    let payload = vec![0x42u8; MAGIC];
    let path = ctx.place("panic.bin", "ITEM_PANIC", &payload)?;
    let errno = ctx.open_errno(&path);
    ctx.fault_fired("KONEDRIVE_FAULT_PANIC_ON_SIZE")?;
    let errno = errno.map_err(|e| format!("a panicking worker left the opener unanswered: {e}"))?;
    if errno != libc::EIO {
        return Err(format!("a panicking worker answered errno {errno}, not EIO"));
    }
    if !ctx.helper_alive() {
        return Err("a panic in one worker killed the helper".into());
    }
    // And the pool still works, at full strength: the next open of an
    // ordinary file goes through.
    let ordinary = ctx.place("after-panic.bin", "ITEM_AFTERPANIC", b"AFTER PANIC")?;
    if ctx.read(&ordinary)? != b"AFTER PANIC" {
        return Err("the pool did not survive a panicking worker".into());
    }
    ctx.restart_helper_with(None, None)?;
    Ok(())
}

/// Review item 11, second half. A panic in a connection's request loop must
/// run the `Disconnect` guard while unwinding: without it the `Daemon` stays
/// in the map, every later hydration for that uid is addressed to a connection
/// nobody reads, and the suspended openers are never denied.
fn connection_panic_contained(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    ctx.restart_helper_with(None, Some(("KONEDRIVE_FAULT_PANIC_ON_MARKFILE", "1")))?;
    let path = ctx.place("conn-panic.bin", "ITEM_CONNPANIC", b"CONN")?;
    ctx.set_source_delay(Duration::from_secs(10));
    let before = ctx.fetches();
    let reader = Reader::start(&ctx.exe, &path)?;
    ctx.wait_for_fetch(before, Duration::from_secs(20))?;

    // The request that panics the connection's thread.
    let file = File::options().read(true).write(true).open(&path).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    let _ = ctx.runtime.block_on(link.mark_file(&file));
    ctx.fault_fired("KONEDRIVE_FAULT_PANIC_ON_MARKFILE")?;

    let errno = reader
        .errno(Duration::from_secs(30))
        .map_err(|e| format!("a panic on the connection left its opener suspended: {e}"))?;
    if errno != libc::EIO {
        return Err(format!("the stranded opener got errno {errno}, not EIO"));
    }
    if !ctx.helper_alive() {
        return Err("a panic on one connection killed the helper".into());
    }
    ctx.source.reset();
    ctx.restart_helper_with(None, None)?;
    let after = ctx.place("after-conn-panic.bin", "ITEM_AFTERCONN", b"RECONNECTED")?;
    if ctx.read(&after)? != b"RECONNECTED" {
        return Err("a new connection does not work after one panicked".into());
    }
    Ok(())
}

/// The original proposal's disk-full scenario: a filesystem with
/// no room left is the only place `pwrite` and `setxattr` both fail, which is
/// what the commit-point ordering in `source::fill` was built
/// for. Whatever fails, the one thing that must never happen is a file that
/// reads `hydrated` over a hole.
fn disk_full(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let image = PathBuf::from(format!("/mnt/img/full-{}.img", ctx.fs));
    let mount = PathBuf::from(format!("/mnt/full-{}", ctx.fs));
    let _ = std::fs::create_dir_all(&mount);
    let _ = std::fs::remove_file(&image);
    // 400 MiB: mkfs.xfs refuses anything under 300 MiB and mkfs.btrfs under
    // about 110 MiB, so one size for all three.
    Command::new("truncate")
        .args(["-s", "400M"])
        .arg(&image)
        .status()
        .map_err(|e| e.to_string())?;
    let mkfs = match ctx.fs {
        "btrfs" => {
            Command::new("mkfs.btrfs").args(["-q", "-f", "-m", "single"]).arg(&image).status()
        }
        "xfs" => Command::new("mkfs.xfs").args(["-q", "-f"]).arg(&image).status(),
        _ => Command::new("mkfs.ext4").args(["-q", "-F"]).arg(&image).status(),
    }
    .map_err(|e| e.to_string())?;
    if !mkfs.success() {
        checks.note(ctx.fs, "disk full", "cannot build a small image of this type; not measured");
        return Ok(());
    }
    if !Command::new("mount")
        .args(["-o", "loop"])
        .arg(&image)
        .arg(&mount)
        .status()
        .map_err(|e| e.to_string())?
        .success()
    {
        return Err("cannot mount the small image".into());
    }

    let result = (|| -> Result<(), String> {
        let small_root = mount.join("root");
        std::fs::create_dir_all(&small_root).map_err(|e| e.to_string())?;
        let link = ctx.link()?;
        let registered = ctx
            .runtime
            .block_on(root::register_root(&link, &small_root))
            .map_err(|e| format!("cannot register the small root: {e}"))?;

        // A placeholder bigger than what is left, and then the rest of the
        // filesystem filled so the fill cannot possibly complete.
        let size = 8 * 1024 * 1024usize;
        let payload = vec![0x77u8; size];
        std::fs::write(ctx.source_dir.join("ITEM_FULL"), &payload).map_err(|e| e.to_string())?;
        let dir = File::open(&small_root).map_err(|e| e.to_string())?;
        create_placeholder(
            &dir,
            "toobig.bin",
            "ITEM_FULL",
            size as u64,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .map_err(|e| format!("cannot create the placeholder: {e}"))?;
        let path = small_root.join("toobig.bin");

        // Filled to the last byte and then given a little back, so the fill
        // writes real data before it runs out. That is the window review
        // item 13 is about: a failure *after* the data has started landing,
        // where `roll_back`'s own `setxattr`, `fallocate` and `ftruncate`
        // are all running on the same full disk.
        let ballast = mount.join("ballast");
        fill_to_the_brim(&ballast)?;
        let freed = 2 * 1024 * 1024u64;
        let held = File::options().write(true).open(&ballast).map_err(|e| e.to_string())?;
        let len = held.metadata().map_err(|e| e.to_string())?.len();
        held.set_len(len.saturating_sub(freed)).map_err(|e| e.to_string())?;
        let _ = held.sync_all();
        drop(held);

        let errno = ctx.open_errno(&path)?;
        if errno != libc::ENOSPC && errno != libc::EIO {
            return Err(format!("a full disk denied with errno {errno}, not ENOSPC or EIO"));
        }
        checks.note(ctx.fs, "disk full", &format!("the open was denied errno {errno}"));

        let state = ctx.state_of(&path)?;
        checks.note(
            ctx.fs,
            "disk full",
            &format!(
                "the file is {state:?} with {} blocks and a {}-byte size; stamp {}",
                std::fs::metadata(&path).map(|m| m.blocks()).unwrap_or(0),
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                match File::open(&path).ok().and_then(|f| read_stamp(&f).ok()) {
                    Some(Some(_)) => "present",
                    Some(None) => "absent",
                    None => "unreadable",
                }
            ),
        );
        if state == Some(State::Hydrated) {
            return Err(
                "a fill that could not finish left the file HYDRATED: the helper will allow and \
                 ignore-mark it, and it reads zeros forever"
                    .into(),
            );
        }
        if !ctx.helper_alive() {
            return Err("the helper did not survive a full disk".into());
        }

        // With room again, the same file fills correctly: the failure left a
        // recoverable state, not a wedged one.
        let _ = std::fs::remove_file(&ballast);
        let content = ctx.read(&path).map_err(|e| {
            format!("the file could not be hydrated once there was room again: {e}")
        })?;
        if content != payload {
            return Err("the retry after freeing space did not fill the file correctly".into());
        }
        ctx.runtime
            .block_on(link.unregister_root(&registered.root_id))
            .map_err(|e| format!("cannot unregister the small root: {e}"))?;
        Ok(())
    })();

    let _ = Command::new("umount").arg(&mount).status();
    let _ = std::fs::remove_file(&image);
    result
}

fn fill_to_the_brim(path: &Path) -> Result<(), String> {
    let mut file = File::create(path).map_err(|e| e.to_string())?;
    let block = vec![0u8; 256 * 1024];
    loop {
        match file.write_all(&block) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => break,
            Err(e) => return Err(format!("cannot fill the image: {e}")),
        }
    }
    // Down to the last byte, so nothing is left for a hydration to use.
    while file.write_all(&[0u8]).is_ok() {}
    let _ = file.sync_all();
    Ok(())
}

/// Review item 14. The host test for this skips silently whenever
/// unprivileged user namespaces are unavailable, so the one place it can be
/// relied on is here, where a real mount needs no namespace at all.
fn cross_device_recovery(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let inner = ctx.root.join("mountpoint");
    let _ = std::fs::remove_dir_all(&inner);
    std::fs::create_dir_all(&inner).map_err(|e| e.to_string())?;
    if !Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=8M", "tmpfs"])
        .arg(&inner)
        .status()
        .map_err(|e| e.to_string())?
        .success()
    {
        return Err("cannot mount a tmpfs inside the root".into());
    }

    let result = (|| -> Result<(), String> {
        // One interrupted file on the other filesystem: recovery must count
        // it as skipped and leave it exactly as it found it.
        let victim = inner.join("victim.bin");
        let file = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&victim)
            .map_err(|e| e.to_string())?;
        file.write_at_all(&vec![1u8; 8192])?;
        write_state(&file, State::Dehydrating).map_err(|e| e.to_string())?;
        drop(file);
        let blocks_before = ctx.blocks_of(&victim)?;

        let link = ctx.link()?;
        let sync_root = ctx.sync_root();
        let report = ctx
            .runtime
            .block_on(root::recover(&Clearance::Link(link), &sync_root, &ctx.locks))
            .map_err(|e| format!("recovery failed: {e}"))?;
        if report.skipped == 0 {
            return Err(format!(
                "a subtree on another filesystem was passed over in silence: {report:?}"
            ));
        }
        if ctx.blocks_of(&victim)? != blocks_before {
            return Err("recovery punched a file on another filesystem".into());
        }
        match ctx.state_of(&victim)? {
            Some(State::Dehydrating) => Ok(()),
            other => Err(format!("a file on another filesystem was changed to {other:?}")),
        }
    })();

    let _ = Command::new("umount").arg(&inner).status();
    let _ = std::fs::remove_dir_all(&inner);
    result
}

trait WriteAtAll {
    fn write_at_all(&self, data: &[u8]) -> Result<(), String>;
}

impl WriteAtAll for File {
    fn write_at_all(&self, data: &[u8]) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        self.write_all_at(data, 0).map_err(|e| e.to_string())
    }
}

#[derive(Debug)]
struct BurstReport {
    answered: usize,
    ok: usize,
    wrong: usize,
    errors: BTreeMap<i32, usize>,
    failed_indices: Vec<usize>,
    /// How long each opener waited, in milliseconds, whatever it got.
    waits_ms: Vec<u64>,
    elapsed: Duration,
    peak_threads: u64,
    peak_rss_kb: u64,
}

impl std::fmt::Display for BurstReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let errs: Vec<String> =
            self.errors.iter().map(|(e, n)| format!("errno {e} x{n}")).collect();
        write!(
            f,
            "answered {}, filled {}, read-wrong {}, [{}] in {:?}; helper peaked at {} threads and \
             {} KiB RSS",
            self.answered,
            self.ok,
            self.wrong,
            errs.join(", "),
            self.elapsed,
            self.peak_threads,
            self.peak_rss_kb
        )?;
        if !self.waits_ms.is_empty() {
            let mut waits = self.waits_ms.clone();
            waits.sort_unstable();
            write!(
                f,
                "; waits: median {} ms, p95 {} ms, max {} ms",
                waits[waits.len() / 2],
                waits[waits.len() * 95 / 100],
                waits[waits.len() - 1]
            )?;
        }
        Ok(())
    }
}

fn run_burst(ctx: &Ctx, dir: &Path, count: usize, expect: u8) -> Result<BurstReport, String> {
    let helper_pid = ctx.helper_pid();
    let sampling = Arc::new(AtomicBool::new(true));
    let peak_threads = Arc::new(AtomicU64::new(0));
    let peak_rss = Arc::new(AtomicU64::new(0));
    let sampler = {
        let sampling = Arc::clone(&sampling);
        let peak_threads = Arc::clone(&peak_threads);
        let peak_rss = Arc::clone(&peak_rss);
        std::thread::spawn(move || {
            while sampling.load(Ordering::SeqCst) {
                if let Some((threads, rss)) = proc_status(helper_pid) {
                    peak_threads.fetch_max(threads, Ordering::SeqCst);
                    peak_rss.fetch_max(rss, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })
    };

    let started = Instant::now();
    let output = Command::new(&ctx.exe)
        .arg("--burst")
        .arg(dir)
        .arg(count.to_string())
        .arg(expect.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run the burst: {e}"))?;
    let elapsed = started.elapsed();
    sampling.store(false, Ordering::SeqCst);
    let _ = sampler.join();

    let said = String::from_utf8_lossy(&output.stdout);
    let mut report = BurstReport {
        answered: 0,
        ok: 0,
        wrong: 0,
        errors: BTreeMap::new(),
        failed_indices: Vec::new(),
        waits_ms: Vec::new(),
        elapsed,
        peak_threads: peak_threads.load(Ordering::SeqCst),
        peak_rss_kb: peak_rss.load(Ordering::SeqCst),
    };
    for line in said.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.first().copied() {
            Some("FAILED") => {
                if let (Some(i), Some(errno)) = (fields.get(1), fields.get(2)) {
                    if let (Ok(i), Ok(errno)) = (i.parse::<usize>(), errno.parse::<i32>()) {
                        report.failed_indices.push(i);
                        *report.errors.entry(errno).or_default() += 1;
                    }
                }
                if let Some(Ok(ms)) = fields.get(3).map(|s| s.parse::<u64>()) {
                    report.waits_ms.push(ms);
                }
            }
            Some("WRONG") => {
                if let Some(Ok(i)) = fields.get(1).map(|s| s.parse::<usize>()) {
                    report.failed_indices.push(i);
                }
                if let Some(Ok(ms)) = fields.get(2).map(|s| s.parse::<u64>()) {
                    report.waits_ms.push(ms);
                }
            }
            Some("TOOK") => {
                if let Some(Ok(ms)) = fields.get(2).map(|s| s.parse::<u64>()) {
                    report.waits_ms.push(ms);
                }
            }
            Some("BURST") => {
                for field in &fields[1..] {
                    if let Some(v) = field.strip_prefix("answered=") {
                        report.answered = v.parse().unwrap_or(0);
                    } else if let Some(v) = field.strip_prefix("ok=") {
                        report.ok = v.parse().unwrap_or(0);
                    } else if let Some(v) = field.strip_prefix("wrong=") {
                        report.wrong = v.parse().unwrap_or(0);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(report)
}

fn proc_status(pid: u32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut threads = 0;
    let mut rss = 0;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Threads:") {
            threads = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    Some((threads, rss))
}


/// The two waiter caps, measured end to end instead of asserted on a counter.
///
/// `wait_for_daemon` is the one place a worker sleeps for tens of seconds, so
/// it is the one lever an unprivileged caller has on the pool.
/// [`MAX_DAEMON_WAITERS`] (8, per uid) and `GLOBAL_MAX_DAEMON_WAITERS` (32,
/// machine-wide) were both chosen rather than measured. With the daemon gone
/// but the root still registered, every open of a placeholder reaches
/// `wait_for_daemon`, so the shape of the answer says exactly which cap binds:
/// an open that waited the full `DAEMON_WAIT` held a slot; one that came back
/// at once was refused by a cap.
fn waiter_caps(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("waiters");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 64usize;
    let payload = vec![0x55u8; 64];
    for i in 0..count {
        ctx.place(&format!("waiters/burst-{i}"), &format!("ITEM_WAITER_{i}"), &payload)?;
    }

    ctx.kill_daemon();
    let outcome = (|| -> Result<(), String> {
        let report = run_burst(ctx, &dir, count, 0x55)?;
        let parked = report.waits_ms.iter().filter(|ms| **ms > 5_000).count();
        let at_once = report.waits_ms.iter().filter(|ms| **ms <= 5_000).count();
        checks.note(
            ctx.fs,
            "waiter caps",
            &format!(
                "with the daemon gone and the root still registered, {count} opens: {parked} \
                 waited the full DAEMON_WAIT (they held a slot) and {at_once} were refused at \
                 once; {report}"
            ),
        );
        if report.answered != count {
            return Err(format!("{} opener(s) never returned", count - report.answered));
        }
        if report.ok != 0 || report.wrong != 0 {
            return Err(format!(
                "{} open(s) succeeded with no daemon to fill them",
                report.ok + report.wrong
            ));
        }
        if parked == 0 {
            return Err("no open waited for the daemon at all; the caps cannot be read off".into());
        }
        if parked >= count {
            return Err(format!(
                "all {count} opens parked: waiting is not bounded, and one user can consume the \
                 whole worker pool"
            ));
        }
        Ok(())
    })();

    if !ctx.daemon_connected() {
        ctx.connect_daemon()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Review item 8, first half: several thousand concurrent opens. The property
/// is not "everything succeeds" — the pool is bounded on purpose, and `EAGAIN`
/// is a real answer that means "try that again" — it is that **every** opener
/// is answered, that none of them reads zeros, and that a retry of the refused
/// ones then works.
fn burst(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("burst");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 3000usize;
    let payload = vec![0x33u8; 4096];
    for i in 0..count {
        ctx.place(&format!("burst/burst-{i}"), &format!("ITEM_BURST_{i}"), &payload)?;
    }

    // Each cause of a refusal, counted through the throttle. The
    // outbox has had two wordings: "is not draining them" while it could fill
    // with requests, "could not be queued" since its request capacity is the
    // credit; and "hydrations outstanding on its connection" is the refusal
    // for want of credit that removed. All are counted so that
    // this scenario says the same thing about a helper from either side of
    // those changes.
    let causes = |log: &Path| {
        (
            occurrences_in_log(log, "workers busy and"),
            occurrences_in_log(log, "is not draining them")
                + occurrences_in_log(log, "could not be queued"),
            occurrences_in_log(log, "hydrations outstanding on its connection"),
        )
    };
    let log = ctx.helper.lock().unwrap().log.clone();
    let lines_before = std::fs::read_to_string(&log).map(|t| t.lines().count()).unwrap_or(0);
    let (queue_full_before, outbox_full_before, saturated_before) = causes(&log);
    let gave_up_before = count_in_log(&log, "cannot write to a daemon");
    let silent_before = count_in_log(&log, "liveness window");
    let stranded_before = count_in_log(&log, "went away with");
    let fetches_before = ctx.fetches();
    let report = run_burst(ctx, &dir, count, 0x33)?;
    if !report.errors.is_empty() {
        // A throttled count is written when its interval is up, not when the
        // refusal happens; the last one of a burst arrives seconds later.
        std::thread::sleep(THROTTLE_SETTLE);
    }
    let (queue_full, outbox_full, saturated) = causes(&log);
    let queue_full = queue_full - queue_full_before;
    let outbox_full = outbox_full - outbox_full_before;
    let saturated = saturated - saturated_before;
    let gave_up = count_in_log(&log, "cannot write to a daemon") - gave_up_before;
    let silent = count_in_log(&log, "liveness window") - silent_before;
    let stranded = count_in_log(&log, "went away with") - stranded_before;
    let started_fills = ctx.fetches() - fetches_before;
    checks.note(ctx.fs, "burst", &format!("{count} concurrent opens: {report}"));
    checks.note(
        ctx.fs,
        "burst",
        &format!(
            "the daemon was asked to start {started_fills} fill(s) and completed {}",
            report.ok
        ),
    );
    // A burst is backpressure, not a wedged daemon, and must not
    // cost the connection. When it does, what the helper said about it is the
    // only account of *why* — its writer giving up for silence, its writer
    // finding the socket gone, or the daemon hanging up — so it is printed.
    let lost_connection = if gave_up > 0 || stranded > 0 {
        checks.note(
            ctx.fs,
            "burst",
            &format!(
                "the CONNECTION WAS LOST: the helper's writer gave up {gave_up} time(s) ({silent} \
                 of them for a whole liveness window of silence) and the helper stranded the \
                 connection's hydrations {stranded} time(s), denying every opener already \
                 enrolled on it EIO"
            ),
        );
        // Only the lines about the connection itself: once it is gone, every
        // open still queued in the pool is refused with a line of its own,
        // and thousands of those bury the one line that says why.
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        for line in text
            .lines()
            .skip(lines_before)
            .filter(|l| {
                ["went away with", "cannot write to a daemon", "connection ended", "liveness"]
                    .iter()
                    .any(|needle| l.contains(needle))
            })
            .take(20)
        {
            println!("  | {line}");
        }
        // The link this side holds is now a link to nothing; every later open
        // would wait the full `DAEMON_WAIT` and then be denied.
        ctx.kill_daemon();
        ctx.connect_daemon()?;
        true
    } else {
        false
    };
    checks.note(
        ctx.fs,
        "burst",
        &format!(
            "refusals by cause: the worker queue was full {queue_full} time(s) \
             (EVENT_WORKERS/EVENT_QUEUE_DEPTH), the connection already had \
             MAX_OUTSTANDING_HYDRATIONS = {} in flight {saturated} time(s), the daemon's outbox \
             refused {outbox_full} time(s)",
            konedrive_proto::MAX_OUTSTANDING_HYDRATIONS,
        ),
    );
    if report.answered != count {
        return Err(format!("{} of {count} openers never returned at all", count - report.answered));
    }
    if report.wrong != 0 {
        return Err(format!(
            "{} opener(s) were allowed onto a file that had not been filled",
            report.wrong
        ));
    }
    if !ctx.helper_alive() {
        return Err("the helper did not survive the burst".into());
    }
    if lost_connection {
        return Err(format!(
            "the burst cost the daemon its connection, and {} enrolled opener(s) were denied EIO \
             (Ruling H119: a burst is backpressure, not a wedged daemon)",
            report.errors.get(&libc::EIO).copied().unwrap_or(0)
        ));
    }
    // Beyond the credit an opener waits for the daemon; it is not
    // refused. The only refusal a burst may still meet is a bound that is
    // genuinely exhausted — the worker pool's queue — so every EAGAIN must be
    // one of those, and nothing else may be refused at all.
    if saturated > 0 {
        return Err(format!(
            "{saturated} opener(s) were refused EAGAIN because their connection already had \
             MAX_OUTSTANDING_HYDRATIONS in flight: beyond the credit an opener must wait, not \
             fail (Ruling H124); {report}"
        ));
    }
    let refused = report.errors.get(&libc::EAGAIN).copied().unwrap_or(0);
    let others: BTreeMap<i32, usize> =
        report.errors.iter().filter(|(e, _)| **e != libc::EAGAIN).map(|(e, n)| (*e, *n)).collect();
    if !others.is_empty() {
        return Err(format!("openers were denied {others:?} (errno -> openers); {report}"));
    }
    if outbox_full > 0 {
        return Err(format!(
            "the daemon's outbox refused {outbox_full} request(s); its request capacity is the \
             credit, so under the credit it cannot fill"
        ));
    }
    if refused > queue_full {
        return Err(format!(
            "{refused} opener(s) were refused EAGAIN, but the worker queue — the one bound that \
             may still refuse a burst — was full only {queue_full} time(s)"
        ));
    }

    // Everything that was refused must succeed when it is tried again: that
    // is what makes `EAGAIN` an answer rather than a failure.
    // Twenty, not two hundred: a retry that is itself refused takes
    // `DAEMON_WAIT` to answer, and a loop of those is how this scenario turned
    // a ten-second measurement into an hour-long hang.
    let mut still_failing = Vec::new();
    for i in report.failed_indices.iter().take(20) {
        let path = dir.join(format!("burst-{i}"));
        match ctx.read(&path) {
            Ok(content) if content == payload => {}
            Ok(_) | Err(_) => {
                still_failing.push(*i);
                break;
            }
        }
    }
    if !still_failing.is_empty() {
        return Err(format!(
            "a refused opener (index {}) still could not be served on a retry",
            still_failing[0]
        ));
    }
    Ok(())
}

/// Review item 8, second half, and the worst outcome this component has.
/// `fanotify(7)`: *"Upon close(2), outstanding permission events will be set
/// to allowed"* — quoted in the design and never measured. So it is measured
/// here: the helper is killed with openers suspended, and what those openers
/// then read is recorded.
fn helper_death_allows(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("death");
    let _ = std::fs::remove_dir_all(&dir);
    ctx.ensure_dir(&dir)?;
    let count = 200usize;
    let payload = vec![0x44u8; 4096];
    for i in 0..count {
        ctx.place(&format!("death/burst-{i}"), &format!("ITEM_DEATH_{i}"), &payload)?;
    }
    // Long enough that every opener is still suspended when the helper dies.
    ctx.set_source_delay(Duration::from_secs(30));

    let killer = {
        let pid = ctx.helper_pid();
        let source = Arc::clone(&ctx.source);
        let before = ctx.fetches();
        std::thread::spawn(move || {
            // Killed once openers are genuinely suspended, not after a fixed
            // sleep: what is being measured is what the kernel does to
            // *outstanding* permission events.
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && source.fetches() <= before {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(500));
            // SAFETY: a plain kill on a pid this process owns.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        })
    };
    let report = run_burst(ctx, &dir, count, 0x44)?;
    let _ = killer.join();

    checks.note(
        ctx.fs,
        "helper death",
        &format!(
            "{count} concurrent opens, of which those still suspended when the helper was \
             killed are the \"read-wrong\" ones ({} — every opener stays suspended now, at \
             most MAX_OUTSTANDING_HYDRATIONS of them sent to the daemon and the rest waiting in \
             the helper for credit): {report} — \
             every \"read-wrong\" open is an application that was handed an unfilled \
             placeholder, which is what fanotify(7) means by \"outstanding permission events \
             will be set to allowed\"",
            report.wrong,
        ),
    );
    if report.answered != count {
        return Err(format!(
            "{} opener(s) were left suspended even after the group fd closed",
            count - report.answered
        ));
    }

    ctx.source.reset();
    // `kill_daemon` first: the link is already dead, and the tasks holding it
    // have to go before a new one is made.
    ctx.kill_daemon();
    {
        let mut helper = ctx.helper.lock().unwrap();
        let _ = helper.child.wait();
        let _ = std::fs::remove_file(SOCKET_PATH);
        helper.child = spawn_helper(&helper.binary.clone(), &helper.log.clone(), None, None)?;
        helper.await_socket()?;
    }
    ctx.connect_daemon()?;
    let after = ctx.place("after-death.bin", "ITEM_AFTERDEATH", b"BACK")?;
    if ctx.read(&after)? != b"BACK" {
        return Err("interception did not come back after the helper was restarted".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// measurement mode
// ---------------------------------------------------------------------------

/// `/proc/slabinfo`, summed as `active_objs × objsize` over every cache, which
/// is what §8's figures are deltas of.
fn slab_total() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/slabinfo") else { return 0 };
    let mut total = 0u64;
    for line in text.lines().skip(2) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }
        let objs: u64 = fields[1].parse().unwrap_or(0);
        let size: u64 = fields[3].parse().unwrap_or(0);
        total += objs * size;
    }
    total
}

fn settled(label: &str) -> u64 {
    // SAFETY: a plain sync(2).
    unsafe { libc::sync() };
    let _ = std::fs::write("/proc/sys/vm/drop_caches", b"3");
    std::thread::sleep(Duration::from_millis(500));
    let total = slab_total();
    println!("  slab after {label}: {total} B");
    total
}

fn measure_mode(helper_binary: &Path, dirs: usize, files: usize) -> i32 {
    println!("== measurement: {dirs} directories, {files} files, on btrfs ==");
    let mount = PathBuf::from("/mnt/btrfs");
    let base = mount.join("measure");
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let source_dir = base.join("source");
    for dir in [&root, &source_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            println!("MEASURE-FAIL cannot create {dir:?}: {e}");
            return 1;
        }
    }

    // The tree, built before the helper knows anything about it, so the
    // startup walk is the thing being timed.
    let started = Instant::now();
    let per_dir = files.div_ceil(dirs.max(1));
    for d in 0..dirs {
        let dir = root.join(format!("d{:05}", d));
        if std::fs::create_dir_all(&dir).is_err() {
            println!("MEASURE-FAIL cannot create {dir:?}");
            return 1;
        }
        let handle = match File::open(&dir) {
            Ok(handle) => handle,
            Err(e) => {
                println!("MEASURE-FAIL cannot open {dir:?}: {e}");
                return 1;
            }
        };
        for f in 0..per_dir {
            let _ = create_placeholder(
                &handle,
                &format!("f{f:04}"),
                &format!("I{d}-{f}"),
                4096,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            );
        }
    }
    println!("  built the tree in {:?}", started.elapsed());

    let _ = std::fs::remove_file(ROOTS_FILE);
    let _ = std::fs::remove_file(SOCKET_PATH);
    let log = PathBuf::from("/run/konedrive-helper-measure.log");
    let _ = std::fs::remove_file(&log);

    let before_marks = settled("the tree was built, before any mark");
    let mut helper = match HelperProc::start(helper_binary, &log) {
        Ok(helper) => helper,
        Err(e) => {
            println!("MEASURE-FAIL {e}");
            return 1;
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let source = TestSource::new(source_dir.clone());
    let (link, requests) = match runtime.block_on(HelperLink::connect(Path::new(SOCKET_PATH))) {
        Ok(pair) => pair,
        Err(e) => {
            println!("MEASURE-FAIL cannot connect: {e}");
            helper.stop();
            return 1;
        }
    };
    let locks = InodeLocks::new();
    runtime.spawn(serve_hydrations(
        link.clone(),
        requests,
        Arc::clone(&source) as Arc<dyn ContentSource>,
        locks,
    ));

    // Registration performs the whole `openat2` walk inside the helper, which
    // is the startup walk under another name.
    // Through `HelperLink` directly, as the DT_UNKNOWN scenario does:
    // `root::register_root` refuses a folder that is not empty,
    // and a tree that already exists is the whole point of timing the walk.
    let root_id = "measure-root";
    let root_handle = match File::open(&root) {
        Ok(handle) => handle,
        Err(e) => {
            println!("MEASURE-FAIL cannot open the root: {e}");
            helper.stop();
            return 1;
        }
    };
    let walk_started = Instant::now();
    if let Err(e) = runtime.block_on(link.register_root(&root_handle, root_id)) {
        println!("MEASURE-FAIL cannot register: {e}");
        helper.stop();
        return 1;
    }
    let walk = walk_started.elapsed();
    println!("  the walk of {dirs} directories took {walk:?}");
    let marks = helper_marks(helper.pid()).len();
    println!("  the group holds {marks} mark(s)");

    let after_marks = settled("every directory is marked");
    let per_mark = (after_marks.saturating_sub(before_marks)) as f64 / marks.max(1) as f64;
    println!("  per directory mark, after drop_caches: {per_mark:.1} B");

    // A first open, a second (ignored) open, and an open outside the root.
    let exe = std::env::current_exe().unwrap();
    let payload = vec![0x21u8; 4096];
    let _ = std::fs::write(source_dir.join("IMEASURE"), &payload);
    let sample = root.join("d00000").join("measured.bin");
    let handle = File::open(root.join("d00000")).unwrap();
    let _ = create_placeholder(
        &handle,
        "measured.bin",
        "IMEASURE",
        payload.len() as u64,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    let outside = base.join("outside.bin");
    let _ = std::fs::write(&outside, &payload);

    let time_open = |path: &Path| -> Duration {
        let started = Instant::now();
        if let Ok(reader) = Reader::start(&exe, path) {
            let _ = reader.get(Duration::from_secs(60));
        }
        started.elapsed()
    };
    // One warm-up so process spawn cost is not what is being compared.
    let _ = time_open(&outside);
    let outside_latency = time_open(&outside);
    let first = time_open(&sample);
    let second = time_open(&sample);
    println!("  first (intercepted, hydrating) open: {first:?}");
    println!("  second (ignore-marked) open:         {second:?}");
    println!("  open of a file outside the root:     {outside_latency:?}");

    let ignore_before = settled("before ignore marks");
    // Ignore-mark a slice of the tree by opening it, then measure.
    let sample_count = 2000.min(files);
    for d in 0..dirs {
        let dir = root.join(format!("d{:05}", d));
        for f in 0..per_dir {
            if d * per_dir + f >= sample_count {
                break;
            }
            let path = dir.join(format!("f{f:04}"));
            let _ = std::fs::write(source_dir.join(format!("I{d}-{f}")), &payload);
            if let Ok(reader) = Reader::start(&exe, &path) {
                let _ = reader.get(Duration::from_secs(30));
            }
        }
        if d * per_dir >= sample_count {
            break;
        }
    }
    let ignored = helper_marks(helper.pid())
        .iter()
        .filter(|m| m.ignored_mask & FAN_OPEN_PERM != 0)
        .count();
    println!("  {ignored} ignore mark(s) after {sample_count} hydrations");
    let ignore_after = settled("with ignore marks in place");
    let per_ignore =
        (ignore_after as i64 - ignore_before as i64) as f64 / ignored.max(1) as f64;
    println!("  per ignore mark, after drop_caches: {per_ignore:.1} B");

    let _ = runtime.block_on(link.unregister_root(root_id));
    helper.stop();
    let _ = std::fs::remove_dir_all(&base);
    0
}
