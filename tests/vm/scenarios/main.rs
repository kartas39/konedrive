//! End-to-end scenarios for the interception path: the real helper binary, the
//! daemon's own `sync` module, and opens driven from child processes.
//!
//! Every scenario prints `PASS`/`FAIL` on its own row; the binary exits
//! non-zero if anything failed. Run as root inside the virtme-ng VM
//! (`tests/vm/run.sh scenarios`).
//!
//! # Why the opens happen in child processes
//!
//! `konedrive-helper` exempts the owning daemon's **pid** from interception:
//! a process that holds a connection owning a registered root
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
//! (`docs/kernel-behavior-7.2/interception.md` §2.1), and `FAN_DENY` with an errno outside
//! a specific set fails the response `write()` and leaves the opener
//! suspended forever (§5). So the assertions here are on observable state:
//! `/proc/<helper>/fdinfo` for whether a mark exists, block counts for
//! whether a file holds data, and what a reader in another process actually
//! gets back.

mod accounts;
mod burst;
mod child;
mod clients;
mod coverage;
mod dehydrate;
mod faults;
mod fills;
mod graph;
mod harness;
mod kernel_facts;
mod lifecycle;
mod measure;
mod move_out;
mod open_by_handle;
mod punch_rule;
mod races;
mod registration;
mod unit;
mod walk;
mod watch;
mod writes;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use konedrive_proto::SOCKET_PATH;
use konedrived::folder::root::{self, SyncRoot};
use konedrived::folder::locks::InodeLocks;

use crate::accounts::two_accounts_one_link;
use crate::burst::{burst, helper_death_allows, helper_stop_denies, waiter_caps};
use crate::child::{
    child_burst, child_connections, child_hold, child_hostile, child_pipeline, child_read,
};
use crate::clients::{
    another_version_is_closed, connections_per_uid_are_capped, errno_sweep, hostile_uid,
    peercred_pid_matches_event_pid,
};
use crate::coverage::{
    hardlink_and_second_mount, moved_out_still_covered, new_directory_covered, zero_byte_file,
};
use crate::dehydrate::{
    clear_ignore_after_reclaim, dehydrate_in_use, dehydrate_then_open, unregistered_ignore_mark,
};
use crate::faults::{
    accept_panic_contained, connection_panic_contained, cross_device_recovery, disk_full,
    emfile_survived, event_loop_panic_contained, worker_panic_contained,
};
use crate::fills::{
    copy_sees_content, failure_rolls_back, instant_reply, killed_reader, mmap_sees_content,
    one_fetch_for_many_openers, open_fills, second_open_is_ignored,
};
use crate::harness::{Checks, Ctx, HelperProc, TestSource};
use crate::kernel_facts::kernel_facts;
use crate::lifecycle::{
    daemon_death_denies, daemon_death_denies_queued, helper_restart_remarks, pipelined_acks,
    transient_connection_hands_back,
};
use crate::measure::measure_mode;
use crate::punch_rule::{
    clear_ignore_by_ownership, punch_without_interception_rule, readonly_mount_open_survived,
    recovery_overtaken_by_old_fill, rename_during_registration_walk,
    running_executable_open_survived,
};
use crate::races::{
    carried_in_ignore_mark, inflight_across_forget, late_ignore_mark,
    leased_file_does_not_stall_others, stale_request_after_direct_fill,
};
use crate::registration::{
    displaced_root_is_unmarked, forget_without_link_refused, no_interception_forget_is_local,
    no_interception_populate_marks_nothing, no_interception_with_helper_connected,
    pending_root_not_downgraded, upgraded_when_the_helper_starts,
};
use crate::walk::{dt_unknown_walk, readonly_mount_event_fd, startup_walk_hazards};

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
        Some("--obh-serve") => std::process::exit(open_by_handle::probe_serve(Path::new(argv[2]))),
        Some("--obh-measure") => {
            std::process::exit(open_by_handle::probe_measure(Path::new(argv[2]), Path::new(argv[3])))
        }
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
    // `--unit <pid> <base> <name>`: a helper systemd already runs, from the
    // shipped unit (`tests/vm/run.sh unit`). See unit.rs.
    let mut unit_target: Option<(u32, PathBuf, String)> = None;
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
            "--unit" => {
                let pid = argv[i + 1].parse().unwrap();
                unit_target = Some((pid, PathBuf::from(argv[i + 2]), argv[i + 3].to_owned()));
                i += 3;
            }
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

    if let Some((helper_pid, base, name)) = unit_target {
        std::process::exit(unit::unit_mode(helper_pid, &base, &name));
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
    // Entered for the whole run: a scenario makes its services outside `block_on`, and a
    // service starts a task of its own as it is made (the queue totals).
    let outcome = {
        let _entered = ctx.runtime.enter();
        run_scenarios(&ctx, &root)
    };
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
        (
            "two accounts' folders on one helper link fill from their own sources, and removing \
             one leaves the other intercepted",
            two_accounts_one_link,
        ),
        ("directory created later is covered", new_directory_covered),
        (
            "watcher: a directory made after the helper's walk is marked by the watcher's",
            watch::bring_up_marks_what_the_helper_missed,
        ),
        (
            "watcher: a directory made and at once given a placeholder is marked within milliseconds, and the open \
             is filled",
            watch::new_directory_marked,
        ),
        (
            "watcher: a tree moved into the folder is marked all the way down before its files are opened",
            watch::tree_moved_in_marked,
        ),
        ("watcher: the daemon's own placement and fill are not handed over", watch::own_fill_is_silent),
        ("watcher: a nested btrfs subvolume is neither watched nor uploaded", watch::other_device_not_uploaded),
        ("file moved out keeps its individual mark", moved_out_still_covered),
        ("a hardlink in an unmarked directory, and a second mount", hardlink_and_second_mount),
        ("zero-byte file needs no fetch", zero_byte_file),
        ("the whole errno space is answered, and nobody is left hanging", errno_sweep),
        ("a hostile uid cannot touch another user's hydrations", hostile_uid),
        (
            "OpenByHandle: a placeholder moved out of the folder is found by its handle, re-marked, filled \
             on open, and ESTALE once deleted",
            open_by_handle::moved_out_placeholder,
        ),
        (
            "OpenByHandle: a directory moved out of the folder is found, and UnmarkDir takes off the mark \
             it took along",
            open_by_handle::moved_out_directory,
        ),
        (
            "OpenByHandle refuses another uid's object, one without the attribute, another device or \
             filesystem, and malformed handles",
            open_by_handle::refusals,
        ),
        (
            "OpenByHandle of a placeholder under the helper's own marks returns at once and fills nothing",
            open_by_handle::own_open_exempt,
        ),
        (
            "move-out: a placeholder moved out of the folder is marked again first, never reads zeros, and is deleted \
             in OneDrive only once it is local",
            move_out::placeholder_moved_out,
        ),
        (
            "move-out: a directory moved out is downloaded where it went, unmarked, and only then deleted in OneDrive",
            move_out::directory_moved_out,
        ),
        (
            "move-out: a placeholder sent to the Trash is removed from it with its .trashinfo, without a download",
            move_out::placeholder_to_the_trash,
        ),
        (
            "move-out: a download that stops part-way deletes nothing; after a restart it is marked again first, then \
             finished",
            move_out::crash_mid_download_then_restart,
        ),
        (
            "writes: a write open of a placeholder fills it first, and the upload carries what was written",
            writes::write_open_fills_then_uploads,
        ),
        (
            "writes: a directory made and at once given a placeholder is marked before the open, and both go up",
            writes::new_directory_marked_then_uploaded,
        ),
        ("writes: a tree moved into the folder is marked all the way down and uploaded", writes::tree_moved_in_marked_and_uploaded),
        ("writes: an upload session left half sent resumes when the daemon starts again", writes::stopped_mid_session_resumes),
        (
            "writes: after a helper restart a Full local scan finds a change no event reported",
            writes::helper_restart_full_scan_finds_a_change,
        ),
        (
            "writes: create, edit, rename, move and delete reach OneDrive, which then matches the folder, and the \
             echo changes nothing",
            writes::round_trip,
        ),
        (
            "writes: a mount that holds no user attributes inside the folder is listed, and does not stop the examination",
            writes::mount_without_attributes_is_passed_over,
        ),
        (
            "writes: a mount that holds no user attributes over a synced folder leaves the folder in OneDrive, and the cycle completes",
            writes::mount_without_attributes_over_a_synced_folder,
        ),
        (
            "writes: a copy that cannot be stripped is passed over, and does not stop the examination",
            writes::copy_that_cannot_be_stripped_is_passed_over,
        ),
        ("one uid cannot hold the helper's connections without bound", connections_per_uid_are_capped),
        ("a Hello with another protocol version closes the connection", another_version_is_closed),
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
        (
            "a root is registered only under a root id, and an id registered onto another \
             directory unmarks the old one",
            displaced_root_is_unmarked,
        ),
        ("a real EMFILE does not end the helper", emfile_survived),
        ("a panic in a worker is contained", worker_panic_contained),
        (
            "a panic on a connection denies its openers and keeps the helper",
            connection_panic_contained,
        ),
        ("a panic in the event loop denies the open in hand and keeps the helper", event_loop_panic_contained),
        ("a panic on the accept thread closes one connection and the next is served", accept_panic_contained),
        ("disk full denies with ENOSPC or EIO and never commits", disk_full),
        ("a subtree on another filesystem is counted, not silently skipped", cross_device_recovery),
        ("the waiter caps bound how many opens may wait for a daemon", waiter_caps),
        ("a burst of several thousand concurrent opens loses nobody", burst),
        ("killing the helper mid-flight allows every suspended open", helper_death_allows),
        ("SIGTERM to the helper mid-flight denies every suspended open", helper_stop_denies),
    ]
}
