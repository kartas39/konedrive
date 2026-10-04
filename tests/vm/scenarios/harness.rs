use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
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
    create_placeholder, read_state, State,
};
use konedrive_proto::{ToDaemon, ToHelper, PROTOCOL_VERSION, SOCKET_PATH};
use konedrived::helper::HelperLink;
use konedrived::folder::root::SyncRoot;
use konedrived::hydration::source::{ContentSource, Fetched, LocalDir, SourceError};
use konedrived::hydration::testing::Faulty;
use konedrived::hydration::server::serve_hydrations;
use konedrived::folder::locks::InodeLocks;

use crate::child::raw_connect;

// ---------------------------------------------------------------------------
// the check ledger
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Checks {
    pub(crate) passed: usize,
    pub(crate) failed: Vec<String>,
    /// One entry per scenario recorded through `record_timed`, so the run can
    /// end with the slowest steps rather than only pass/fail counts. Kernel
    /// facts and the "helper still running" check go through plain `record`
    /// and never appear here — they are not scenarios with a wait budget of
    /// their own.
    pub(crate) timings: Vec<(String, Duration)>,
}

impl Checks {
    pub(crate) fn record(&mut self, fs: &str, name: &str, outcome: Result<(), String>) {
        self.record_timed(fs, name, outcome, None);
    }

    /// Same as `record`, but appends each `ok`/`FAIL` line with how long the
    /// step took (e.g. `ok    [btrfs] name (1.84 s)`) and, when `elapsed` is
    /// given, remembers it for the end-of-run slowest-steps list.
    pub(crate) fn record_timed(
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

    pub(crate) fn note(&mut self, fs: &str, name: &str, what: &str) {
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
pub(crate) struct Mark {
    pub(crate) ino: u64,
    mask: u64,
    pub(crate) ignored_mask: u64,
    pub(crate) mflags: u64,
}

pub(crate) fn helper_marks(pid: u32) -> Vec<Mark> {
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

pub(crate) fn parse_mark(line: &str) -> Option<Mark> {
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

pub(crate) const FAN_OPEN_PERM: u64 = 0x0001_0000;

pub(crate) fn ignore_mark_present(pid: u32, ino: u64) -> bool {
    helper_marks(pid)
        .iter()
        .any(|m| m.ino == ino && m.ignored_mask & FAN_OPEN_PERM != 0)
}

pub(crate) fn dir_mark_present(pid: u32, ino: u64) -> bool {
    helper_marks(pid).iter().any(|m| m.ino == ino && m.mask & FAN_OPEN_PERM != 0)
}


// ---------------------------------------------------------------------------
// the helper process
// ---------------------------------------------------------------------------

pub(crate) struct HelperProc {
    pub(crate) child: Child,
    pub(crate) binary: PathBuf,
    pub(crate) log: PathBuf,
    /// A hard `RLIMIT_NOFILE` for the helper alone, for the `EMFILE` scenario.
    nofile: Option<u64>,
    fault: Option<(String, String)>,
}

impl HelperProc {
    pub(crate) fn start(binary: &Path, log: &Path) -> Result<Self, String> {
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

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub(crate) fn stop(&mut self) {
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
    pub(crate) fn await_socket(&mut self) -> Result<(), String> {
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

pub(crate) fn spawn_helper(
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
pub(crate) fn count_in_log(log: &Path, needle: &str) -> usize {
    std::fs::read_to_string(log)
        .map(|text| text.lines().filter(|line| line.contains(needle)).count())
        .unwrap_or(0)
}

/// How many times the helper did something it reports through a throttle
///. A throttled line stands for as many occurrences as it says
/// it does; any other line stands for itself, which is also what every line
/// meant before the throttle existed — so this counts correctly against a
/// helper from either side of that change.
pub(crate) fn occurrences_in_log(log: &Path, needle: &str) -> usize {
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
pub(crate) const UNOPENABLE: &str = "could not hand over";

/// How long a throttled count can wait to be written: the helper's report
/// interval plus its flusher's tick, with room to spare.
pub(crate) const THROTTLE_SETTLE: Duration = Duration::from_secs(8);

pub(crate) fn tail(log: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(log) else { return "<no log>".into() };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(12);
    lines[start..].join(" | ")
}

// ---------------------------------------------------------------------------
// the content source
// ---------------------------------------------------------------------------

/// `LocalDir` wrapped in a counting source whose fault injection can be
/// changed between scenarios: a fresh `Faulty` is built per fetch from the
/// current settings (its knobs are builder methods that consume it), and the
/// counting is done here.
pub(crate) struct TestSource {
    pub(crate) dir: PathBuf,
    fetches: AtomicU64,
    delay_ms: AtomicU64,
    /// Bytes after which the stream breaks, or `-1` for a source that works.
    pub(crate) fail_at: AtomicI64,
    /// Whether the break clears itself after one failed fetch.
    pub(crate) fail_once: AtomicBool,
}

impl TestSource {
    pub(crate) fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            dir,
            fetches: AtomicU64::new(0),
            delay_ms: AtomicU64::new(0),
            fail_at: AtomicI64::new(-1),
            fail_once: AtomicBool::new(false),
        })
    }

    pub(crate) fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }

    pub(crate) fn reset(&self) {
        self.delay_ms.store(0, Ordering::SeqCst);
        self.fail_at.store(-1, Ordering::SeqCst);
        self.fail_once.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl ContentSource for TestSource {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let mut local = Faulty::new(LocalDir::new(self.dir.clone()));
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
        local.fetch(item_id, from, end).await
    }
}

/// A second daemon connection from this uid that answers every hydration
/// request with one errno, for as long as it lives. This is how the errno
/// space is swept: a fill clamps every value it produces, so the
/// only way to hand the helper an arbitrary one is to bypass the fill.
///
/// A connection of its own, and not an override in front of the real
/// daemon's request queue, because that override was a forwarding task with
/// a 64-deep channel of its own, and every hydration in the suite went
/// through it: the 3000-open burst measured the daemon with twice its real
/// buffering. As the newest connection of the uid, this one is
/// where the helper sends hydrations; dropped, it hands them
/// back to the daemon underneath.
pub(crate) struct Responder {
    errno: Arc<std::sync::atomic::AtomicI32>,
    control: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Responder {
    pub(crate) fn start() -> Result<Self, String> {
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

    pub(crate) fn answer_with(&self, errno: i32) {
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

pub(crate) struct Reader {
    pid: i32,
    started: Instant,
    /// What the reader got, and when it had finished.
    outcome: mpsc::Receiver<(Result<Vec<u8>, i32>, Instant)>,
}

impl Reader {
    pub(crate) fn start(exe: &Path, path: &Path) -> Result<Self, String> {
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

    pub(crate) fn kill(&self) {
        // SAFETY: a plain kill on a pid this process owns.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
    }

    pub(crate) fn get(&self, within: Duration) -> Result<Result<Vec<u8>, i32>, String> {
        self.get_timed(within).map(|(result, _)| result)
    }

    /// What the reader got, and how long after it was started it had it.
    pub(crate) fn get_timed(&self, within: Duration) -> Result<(Result<Vec<u8>, i32>, Duration), String> {
        self.outcome
            .recv_timeout(within)
            .map(|(result, at)| (result, at.duration_since(self.started)))
            .map_err(|e| format!("the reader never returned within {within:?}: {e}"))
    }

    /// [`get_timed`](Self::get_timed), or `None` if the reader is still
    /// waiting after `within` — in which case it can still be asked again.
    pub(crate) fn poll(&self, within: Duration) -> Option<(Result<Vec<u8>, i32>, Duration)> {
        self.outcome
            .recv_timeout(within)
            .ok()
            .map(|(result, at)| (result, at.duration_since(self.started)))
    }

    /// The content, or a description of why there is none.
    pub(crate) fn content(&self, within: Duration) -> Result<Vec<u8>, String> {
        match self.get(within)? {
            Ok(content) => Ok(content),
            Err(errno) => Err(format!("the open failed with errno {errno}")),
        }
    }

    /// The errno, or a complaint that the open succeeded.
    pub(crate) fn errno(&self, within: Duration) -> Result<i32, String> {
        match self.get(within)? {
            Ok(content) => Err(format!("the open succeeded, returning {} bytes", content.len())),
            Err(errno) => Ok(errno),
        }
    }
}

/// A second descriptor on a file, held by another process, which is what
/// `F_SETLEASE` refuses.
pub(crate) struct Holder {
    child: Child,
}

impl Holder {
    pub(crate) fn start(exe: &Path, path: &Path) -> Result<Self, String> {
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

    pub(crate) fn release(mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// the harness
// ---------------------------------------------------------------------------

pub(crate) struct Daemon {
    link: HelperLink,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub(crate) struct Ctx {
    pub(crate) fs: &'static str,
    pub(crate) exe: PathBuf,
    /// The registered sync root.
    pub(crate) root: PathBuf,
    /// Where `LocalDir` finds a payload for each item id.
    pub(crate) source_dir: PathBuf,
    /// A directory on the same filesystem, outside the root.
    pub(crate) outside: PathBuf,
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) helper: Mutex<HelperProc>,
    pub(crate) daemon: Mutex<Option<Daemon>>,
    pub(crate) sync_root: Mutex<SyncRoot>,
    pub(crate) source: Arc<TestSource>,
    pub(crate) locks: InodeLocks,
    /// Directories already announced to the helper, so a scenario that places
    /// three thousand placeholders in one directory does not send three
    /// thousand `MarkDir` requests. Cleared whenever the helper restarts,
    /// because its marks do not survive that on their own — the startup walk
    /// puts them back, but only for directories inside a registered root.
    pub(crate) marked: Mutex<std::collections::HashSet<PathBuf>>,
}

impl Ctx {
    pub(crate) fn helper_pid(&self) -> u32 {
        self.helper.lock().unwrap().pid()
    }

    pub(crate) fn link(&self) -> Result<HelperLink, String> {
        self.daemon
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.link.clone())
            .ok_or_else(|| "the daemon is not connected".to_string())
    }

    pub(crate) fn sync_root(&self) -> SyncRoot {
        self.sync_root.lock().unwrap().clone()
    }

    pub(crate) fn fetches(&self) -> u64 {
        self.source.fetches()
    }

    pub(crate) fn set_source_delay(&self, delay: Duration) {
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
    pub(crate) fn wait_for_fetch(&self, before: u64, within: Duration) -> Result<(), String> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.fetches() > before {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(format!("no hydration had begun within {within:?}"))
    }

    pub(crate) fn daemon_connected(&self) -> bool {
        self.daemon.lock().unwrap().is_some()
    }

    /// A placeholder at `rel` inside the root, and the payload that fills it.
    /// Intermediate directories are created and marked, exactly as the daemon
    /// does while populating (invariant M1: marked before anything is created
    /// inside them).
    pub(crate) fn place(&self, rel: &str, item_id: &str, content: &[u8]) -> Result<PathBuf, String> {
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
    pub(crate) fn ensure_dir(&self, path: &Path) -> Result<(), String> {
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

    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>, String> {
        let reader = Reader::start(&self.exe, path)?;
        reader.content(Duration::from_secs(60))
    }

    pub(crate) fn open_errno(&self, path: &Path) -> Result<i32, String> {
        let reader = Reader::start(&self.exe, path)?;
        reader.errno(Duration::from_secs(60))
    }

    pub(crate) fn state_of(&self, path: &Path) -> Result<Option<State>, String> {
        let file = File::open(path).map_err(|e| format!("cannot open {path:?}: {e}"))?;
        read_state(&file).map_err(|e| format!("cannot read the state of {path:?}: {e}"))
    }

    pub(crate) fn blocks_of(&self, path: &Path) -> Result<u64, String> {
        std::fs::metadata(path).map(|m| m.blocks()).map_err(|e| e.to_string())
    }

    pub(crate) fn ino_of(&self, path: &Path) -> Result<u64, String> {
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
    pub(crate) fn holds_no_data(&self, path: &Path) -> Result<(), String> {
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
    pub(crate) fn kill_daemon(&self) {
        if let Some(daemon) = self.daemon.lock().unwrap().take() {
            for task in &daemon.tasks {
                task.abort();
            }
            drop(daemon);
        }
        // Give the helper a moment to notice and drain the connection's jobs.
        std::thread::sleep(Duration::from_millis(200));
    }

    pub(crate) fn connect_daemon(&self) -> Result<(), String> {
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

    pub(crate) fn restart_helper(&self) -> Result<(), String> {
        self.kill_daemon();
        self.marked.lock().unwrap().clear();
        self.helper.lock().unwrap().restart()?;
        self.connect_daemon()
    }

    /// Restarts the helper with a descriptor limit, or a fault armed, or
    /// neither.
    pub(crate) fn restart_helper_with(
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

    pub(crate) fn helper_alive(&self) -> bool {
        self.helper.lock().unwrap().alive()
    }

    /// Whether an armed fault really went off. Without it, a helper built
    /// without the `fault-injection` feature would make both
    /// unwind scenarios fail with a message about the unwind path — the one
    /// thing they would not have exercised at all.
    pub(crate) fn fault_fired(&self, name: &str) -> Result<(), String> {
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

    pub(crate) fn helper_log_tail(&self) -> String {
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
