//! The real account: a folder listed from the
//! user's OneDrive, opens that download and verify, and resumes — against
//! Microsoft's servers, with a short-lived read-only token. Btrfs only.
//! Every call goes through `DriveClient`, which sends only GET; the token is
//! `Files.Read`, so Microsoft refuses anything else anyway.
//!
//! Safety, all binding:
//! - only GET requests reach Graph (`DriveClient` never issues anything
//!   else — see `crates/konedrived/src/drive/mod.rs`);
//! - nothing is written to the cloud;
//! - the token file is read only inside the guest, never copied, logged or
//!   printed — [`check_token_permissions`] refuses one that is not mode
//!   `0600` and owned by the user running this binary, before a byte of it
//!   is read;
//! - no refresh token anywhere — [`StaticToken`] cannot refresh;
//! - the scenarios use a temporary folder inside the guest
//!   (`/mnt/btrfs/graph`), never the user's real OneDrive folder.
//!
//! A real run must also pass `--graph-folder <path in OneDrive>` — required,
//! and a real folder: not empty, no `..`, a leading `/` allowed; a run whose
//! folder holds no placed item FAILS — see [`Scope::of`] — and may pass
//! `--graph-max-bytes <n>` (default 32 MiB, no minimum since it has a
//! sensible default of its own). G3 and G4, which download a file of up to
//! that size twice, run only with `--graph-resume-checks`
//! (W15: never by default). No thumbnail filler runs: it would fetch a
//! thumbnail of every image in the drive.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use konedrive_fs::placeholder::{read_ctag, read_progress, read_state, State};
use konedrived::drive::DriveClient;
use konedrived::quickxor::QuickXor;
use konedrived::state::{AccountSnapshot, SignInState, StateHandle};
use konedrived::sync::graph_source::GraphSource;
use konedrived::sync::helper::Clearance;
use konedrived::sync::source::{ContentSource, Fetched, SourceError};
use konedrived::sync::{root, SyncPaths, SyncService};
use konedrived::token::StaticToken;
use konedrived::tree::{Kind, Placement, Row, Table, TreeStore};
use tokio::io::{AsyncRead, ReadBuf};

use crate::HelperProc;

const GRAPH: &str = "https://graph.microsoft.com/v1.0/";
const MIB: u64 = 1 << 20;
/// `--graph-max-bytes`'s default: caps every file any scenario downloads,
/// when a future real run does not set its own.
pub(crate) const DEFAULT_MAX_BYTES: u64 = 32 * MIB;

/// What a real-account run is limited to (dispatch, "the real run" follow-up:
/// no more whole-drive runs; a future one must scope to one folder and cap
/// download size). `folder` restricts which of the listing's rows G2/G3/G4
/// verify by content or download — filtered in, once, right after G1 — to
/// one OneDrive subtree; `max_bytes` is a hard ceiling on the size of any
/// file any of them downloads, folded into every size window below.
///
/// `folder` does **not** narrow the Graph listing itself: `DriveClient` has
/// no item-scoped delta or children-listing method today (only
/// `delta(DeltaFrom::Start)`, the whole drive, and `item(id)`, one item by
/// id — see `crates/konedrived/src/drive/mod.rs`). Adding one is out of
/// scope for this pass, which touches only `tests/vm/` and docs, not
/// `crates/konedrived` (another agent is editing there). So G1 still lists
/// and materializes the whole drive; `folder` is enforced at the point
/// every later scenario decides what to touch, which is what keeps
/// downloads inside it — the property the dispatch actually asked for.
#[derive(Clone)]
pub(crate) struct Scope {
    /// Relative to the synthetic `OneDrive` folder, the same paths
    /// `placed_rows` returns.
    folder: PathBuf,
    max_bytes: u64,
    /// G3 and G4 run (`--graph-resume-checks`).
    resume_checks: bool,
}

/// What the command line said of a real-account run, before it is checked
/// ([`Scope::of`]).
pub(crate) struct Args {
    /// `--graph-folder`, as given.
    pub(crate) folder: Option<String>,
    pub(crate) max_bytes: u64,
    pub(crate) resume_checks: bool,
}

impl Scope {
    /// The run's scope, or why it is refused: `--graph-folder` must name a
    /// real folder — not missing or empty, no `..` or `.` anywhere, at least
    /// one name; a leading `/` is dropped (the path is inside OneDrive
    /// either way).
    pub(crate) fn of(args: Args) -> Result<Scope, String> {
        use std::path::Component;
        let Some(given) = args.folder else {
            return Err(
                "--graph-token needs --graph-folder <path in OneDrive> too: a real run must be scoped to one folder, not the whole drive".into(),
            );
        };
        let folder = PathBuf::from(given.trim_start_matches('/'));
        let mut names = 0;
        for component in folder.components() {
            match component {
                Component::Normal(_) => names += 1,
                _ => return Err(format!("--graph-folder {given:?} must be a plain path inside OneDrive, with no `..` or `.`")),
            }
        }
        if names == 0 {
            return Err(format!("--graph-folder {given:?} names no folder: a real run must be scoped to one folder, not the whole drive"));
        }
        Ok(Scope { folder, max_bytes: args.max_bytes, resume_checks: args.resume_checks })
    }

    fn in_scope(&self, rel: &Path) -> bool {
        rel == self.folder || rel.starts_with(&self.folder)
    }

    /// `rows`, filtered to this scope's folder (or all of them, unscoped).
    fn rows_in_scope(&self, rows: &[(Row, PathBuf)]) -> Vec<(Row, PathBuf)> {
        rows.iter().filter(|(_, rel)| self.in_scope(rel)).cloned().collect()
    }
}

/// Records where each fetch starts; breaks one stream after `break_at`
/// (an offset in the file) when asked to.
struct Watched {
    inner: Arc<dyn ContentSource>,
    froms: Mutex<Vec<u64>>,
    break_at: Mutex<Option<u64>>,
    /// When true, `break_at` is never cleared: every fetch — including every
    /// retry after a break — breaks again at the same absolute offset.
    /// dispatch, Step 4's second guard: it proves the scenario sees
    /// a resume that genuinely cannot succeed as a failure, not as an
    /// endless silent retry.
    sticky: bool,
}

impl Watched {
    fn new(inner: Arc<dyn ContentSource>, break_at: Option<u64>, sticky: bool) -> Arc<Self> {
        Arc::new(Self { inner, froms: Mutex::new(Vec::new()), break_at: Mutex::new(break_at), sticky })
    }

    fn froms(&self) -> Vec<u64> {
        self.froms.lock().unwrap().clone()
    }
}

#[async_trait]
impl ContentSource for Watched {
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
        self.froms.lock().unwrap().push(from);
        let mut fetched = self.inner.fetch(item_id, from).await?;
        let at = if self.sticky { *self.break_at.lock().unwrap() } else { self.break_at.lock().unwrap().take() };
        if let Some(at) = at {
            fetched.stream = Box::new(BreakAfter { inner: fetched.stream, left: at.saturating_sub(fetched.served_from) });
        }
        Ok(fetched)
    }
}

/// Strips the fetch's own `quickXorHash` claim before it reaches the daemon.
/// dispatch, Step 4's first guard: it proves G2's own
/// re-verification — comparing the downloaded bytes' hash against the row's
/// `quickxor` from the *listing*, independent of whatever this fetch
/// says — is what actually holds, not the source's word.
struct NoHash(Arc<dyn ContentSource>);

#[async_trait]
impl ContentSource for NoHash {
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
        let mut fetched = self.0.fetch(item_id, from).await?;
        if let Some(v) = fetched.version.as_mut() {
            v.quick_xor = None;
        }
        Ok(fetched)
    }
}

struct BreakAfter {
    inner: Box<dyn AsyncRead + Send + Unpin>,
    left: u64,
}

impl AsyncRead for BreakAfter {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.left == 0 {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "a break the VM suite injected")));
        }
        let max = (self.left as usize).min(buf.remaining());
        // As tokio's own `Take` does it.
        let mut limited = buf.take(max);
        let start = limited.filled().as_ptr();
        ready!(Pin::new(&mut self.inner).poll_read(cx, &mut limited))?;
        assert_eq!(start, limited.filled().as_ptr());
        let n = limited.filled().len();
        // SAFETY: `limited` initialised these `n` bytes of `buf`'s unfilled part.
        unsafe { buf.assume_init(n) };
        buf.advance(n);
        self.left -= n as u64;
        Poll::Ready(Ok(()))
    }
}

fn report(name: &str, outcome: Result<Option<String>, String>) -> bool {
    match outcome {
        Ok(None) => { println!("  PASS  {name}"); true }
        Ok(Some(why)) => { println!("  SKIP  {name}: {why}"); true }
        Err(why) => { println!("  FAIL  {name}: {why}"); false }
    }
}

/// Reads `path` whole in a child process, so the helper intercepts the open.
fn read_in_child(path: &Path) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = Command::new(exe).arg("--read").arg(path).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("reading {} failed with errno {:?}", path.display(), out.status.code()));
    }
    Ok(out.stdout)
}

fn quickxor(bytes: &[u8]) -> String {
    let mut h = QuickXor::new();
    h.update(bytes);
    h.finish_base64()
}

/// Every placed row below the root, with its path, walking the committed tree.
fn placed_rows(store: &TreeStore) -> Result<Vec<(Row, PathBuf)>, String> {
    let root = store.root_item_id().map_err(|e| e.to_string())?.ok_or("no root in the tree")?;
    let mut out = Vec::new();
    let mut queue = vec![(root, PathBuf::new())];
    while let Some((id, rel)) = queue.pop() {
        for row in store.children(Table::Items, &id).map_err(|e| e.to_string())? {
            if row.placement != Placement::Placed {
                continue;
            }
            let child = rel.join(&row.name);
            if row.kind == Kind::Folder {
                queue.push((row.id.clone(), child.clone()));
            }
            out.push((row, child));
        }
    }
    Ok(out)
}

/// Refuses a token file that is not mode `0600` or not owned by the uid this
/// binary runs as (root, inside the guest). The token is read-only and
/// short-lived either way; this is a belt-and-braces check against picking
/// up a token some other account (or process) on the host side left lying
/// around world- or group-readable — nothing here weakens what Graph itself
/// enforces (`Files.Read`, no write scope, no refresh token).
///
/// This runs as guest root (`run.sh` always boots `vng --user root`), and the
/// token file lives on the host, shared in over virtiofs with its host
/// ownership intact (verified: `stat` in the guest reports the host user's
/// uid, not 0) — so "owned by the user" cannot mean "owned by this
/// process's own uid" the way it would outside a VM; it means owned by an
/// account that is not root. That is the same thing every real run of this
/// mode sees: `konedrivectl dev export-access-token` runs as the desktop
/// user, never as root, so the file it writes is never uid 0.
fn check_token_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink, refusing to follow it for a token file", path.display()));
    }
    let mode = meta.mode() & 0o777;
    if mode != 0o600 {
        return Err(format!(
            "{} is mode {mode:03o}, refusing to read a token file that is not exactly 0600",
            path.display()
        ));
    }
    if meta.uid() == 0 {
        return Err(format!(
            "{} is owned by root; refusing to read a token file that is not the user's own",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn graph_mode(helper_binary: &Path, token_file: &Path, guard: Option<&str>, args: Args) -> i32 {
    // A real-account run must be scoped to one real folder.
    // Checked first, and checked even with no real token behind it — this is
    // the one guard the `--graph-token /nonexistent` verification run (which
    // never reaches Graph) can and does exercise.
    let scope = match Scope::of(args) {
        Ok(scope) => scope,
        Err(why) => {
            println!("FAIL {why}");
            return 1;
        }
    };
    if let Err(why) = check_token_permissions(token_file) {
        println!("FAIL {why}");
        return 1;
    }
    let token = match std::fs::read_to_string(token_file) {
        Ok(token) => token.trim().to_owned(),
        Err(e) => {
            println!("FAIL cannot read the access token at {}: {e}", token_file.display());
            return 1;
        }
    };
    if token.is_empty() {
        println!("FAIL {} is empty", token_file.display());
        return 1;
    }
    let base = PathBuf::from("/mnt/btrfs/graph");
    let _ = std::fs::remove_dir_all(&base);
    let folder = base.join("OneDrive");
    std::fs::create_dir_all(&folder).unwrap();
    let _ = std::fs::remove_file(crate::ROOTS_FILE);
    let _ = std::fs::remove_file(konedrive_proto::SOCKET_PATH);
    let log = PathBuf::from("/run/konedrive-helper-graph.log");
    let mut helper = match HelperProc::start(helper_binary, &log) {
        Ok(helper) => helper,
        Err(why) => {
            println!("FAIL the helper did not start: {why}");
            return 1;
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let passed = runtime.block_on(scenarios(&token, &base, &folder, guard, scope));
    helper.stop();
    if passed { 0 } else { crate::print_helper_log(&log); 1 }
}

async fn scenarios(token: &str, base: &Path, folder: &Path, guard: Option<&str>, scope: Scope) -> bool {
    let account = StateHandle::new(AccountSnapshot { state: SignInState::SignedIn, ..AccountSnapshot::default() });
    let service = SyncService::new(None, Some(account), Some(base.join("config.toml")));
    let drive = DriveClient::new(url::Url::parse(GRAPH).unwrap(), Arc::new(StaticToken::new(token))).unwrap();
    service.set_drive(drive.clone());
    // No thumbnail filler (`thumbnails: None`): it would fetch a thumbnail
    // of every image in the whole drive.
    service.set_sync_paths(SyncPaths { tree_db: base.join("tree.sqlite"), rescue_dir: base.join("rescued"), thumbnails: None });
    tokio::spawn(konedrived::sync::supervise_helper(Arc::clone(&service), konedrive_proto::SOCKET_PATH.into(), Duration::from_secs(1)));
    let deadline = Instant::now() + Duration::from_secs(30);
    while service.link().is_none() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut all = true;
    all &= report("G1 the drive is listed", g1(&service, base, folder).await);
    let store = TreeStore::open(&base.join("tree.sqlite")).expect("the tree store");
    let all_rows = placed_rows(&store).unwrap_or_default();
    let rows = scope.rows_in_scope(&all_rows);
    println!(
        "        scoped to {} ({} of {} placed rows), capped at {} bytes per download",
        scope.folder.display(),
        rows.len(),
        all_rows.len(),
        scope.max_bytes
    );
    // A folder that holds nothing placed is a mistyped or missing folder,
    // not a scenario to skip: every check after this would SKIP and the run
    // would read as passing.
    if rows.is_empty() {
        return report(
            "the run's folder holds placed items",
            Err(format!("no placed item is inside {}: is it a folder in this OneDrive?", scope.folder.display())),
        );
    }

    // dispatch, Step 4: the two guards, each its own one-off
    // invocation (`--graph-guard no-hash` / `--graph-guard
    // permanent-break`), never both at once and never on by default.
    if guard == Some("no-hash") {
        let hashless: Arc<dyn ContentSource> = Arc::new(NoHash(Arc::new(GraphSource::new(drive.clone()))));
        service.replace_content_source(hashless);
        return report("G2 opening downloads and verifies (guard: no quick_xor from the source)", g2(folder, &rows, scope.max_bytes));
    }

    all &= report("G2 opening downloads and verifies", g2(folder, &rows, scope.max_bytes));
    let graph: Arc<dyn ContentSource> = Arc::new(GraphSource::new(drive));

    if guard == Some("permanent-break") {
        return report(
            "G3 a dropped connection resumes with a range (guard: the break never clears)",
            g3(&service, &graph, folder, &rows, true, scope.max_bytes),
        );
    }

    // W15: never by default against the real account —
    // each downloads a file of up to `--graph-max-bytes`, twice.
    if !scope.resume_checks {
        let why = || Ok(Some("not asked for: pass --graph-resume-checks to run it against the real account".to_owned()));
        report("G3 a dropped connection resumes with a range", why());
        report("G4 a restart resumes from the checkpoint", why());
        return all;
    }
    all &= report("G3 a dropped connection resumes with a range", g3(&service, &graph, folder, &rows, false, scope.max_bytes));
    all &= report("G4 a restart resumes from the checkpoint", g4(&service, &graph, folder, &rows, scope.max_bytes).await);
    all
}

async fn g1(service: &Arc<SyncService>, base: &Path, folder: &Path) -> Result<Option<String>, String> {
    let started = Instant::now();
    service.register_root(folder).await.map_err(|e| e.to_string())?;
    let deadline = started + Duration::from_secs(30 * 60);
    loop {
        let state = service.root_state();
        let (listed, placed, skipped) = service.items();
        // `listing` (and so `root_state()`) flips away from "listing" the
        // moment the delta feed finishes paging — before the reconcile that
        // follows has materialized a single placeholder. At real scale (tens
        // of thousands of items) that gap is wide enough for a 10 s poll to
        // land inside it: observed here once, `listed` already nonzero from
        // paging while `placed`/`skipped` were still 0, stale from before
        // this cycle. Comparing `placed + skipped` against `listed` does not
        // close that window either: items inside a *skipped* folder count
        // toward neither (they are never reached, so never tallied either
        // way — see the `skipped 5`/`skipped onenote: …` case, where the
        // folder itself is one `skipped`, its contents are none of the
        // three), so an exact reconciliation can undercount `listed` by
        // design, not just transiently.
        //
        // `last_checked` (`status().0`) is unambiguous: it moves exactly
        // once, at the very end of a cycle that actually succeeded — after
        // listing, reconcile and `publish_counts` — and starts at 0 ("never")
        // for a fresh base directory, which this always is. Waiting for it
        // to leave 0 is waiting for the whole cycle, not just the paging.
        let (last_checked, _, _) = service.status();
        if state != "listing" && last_checked > 0 {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!(
                "still {state} after 30 minutes ({listed} listed, {placed} placed, {skipped} skipped, last_checked={last_checked})"
            ));
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
        println!("        listing: {listed} items so far, {placed} placed, {skipped} skipped ({:?})", started.elapsed());
    }
    let (listed, placed, skipped) = service.items();
    println!("        listed {listed}, placed {placed}, skipped {skipped} in {:?}", started.elapsed());
    let skipped_list = service.skipped().await.map_err(|e| e.to_string())?;
    for (path, reason) in &skipped_list {
        println!("        skipped {reason}: {path}");
    }
    let store = TreeStore::open(&base.join("tree.sqlite")).map_err(|e| e.to_string())?;
    let rows = placed_rows(&store)?;
    if rows.len() as u64 != placed {
        return Err(format!("{} placed rows, ItemsPlaced says {placed}", rows.len()));
    }
    let entries = walkdir_count(folder)?;
    if entries != placed {
        return Err(format!("{entries} entries in the folder, {placed} placed"));
    }
    for (row, rel) in &rows {
        let meta = std::fs::symlink_metadata(folder.join(rel)).map_err(|e| format!("{}: {e}", rel.display()))?;
        use std::os::unix::fs::MetadataExt;
        if row.kind == Kind::File && (meta.len() != row.size || meta.mtime() != row.mtime) {
            return Err(format!("{}: {} bytes at {}, the tree says {} at {}", rel.display(), meta.len(), meta.mtime(), row.size, row.mtime));
        }
    }
    let vault_by_name = rows.iter().any(|(row, rel)| row.kind == Kind::Folder && rel.as_os_str() == "Personal Vault");
    if vault_by_name {
        return Err("a folder named Personal Vault was placed: the vault's detection (specialFolder.name == \"vault\") is wrong".into());
    }
    Ok(None)
}

fn walkdir_count(folder: &Path) -> Result<u64, String> {
    let mut count = 0;
    let mut pending = vec![folder.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            count += 1;
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(count)
}

fn g2(folder: &Path, rows: &[(Row, PathBuf)], max_bytes: u64) -> Result<Option<String>, String> {
    let cap = (20 * MIB).min(max_bytes);
    let mut chosen: Vec<&(Row, PathBuf)> = rows
        .iter()
        .filter(|(r, _)| r.kind == Kind::File && (1024..=cap).contains(&r.size) && r.quickxor.is_some())
        .collect();
    chosen.sort_by_key(|(r, _)| !(r.mime.as_deref().is_some_and(|m| m.starts_with("image/") || m == "application/pdf")));
    chosen.truncate(5);
    if chosen.is_empty() {
        return Ok(Some(format!("no file between 1 KiB and {cap} bytes")));
    }
    for (row, rel) in chosen {
        let path = folder.join(rel);
        let bytes = read_in_child(&path)?;
        if bytes.len() as u64 != row.size {
            return Err(format!("{}: read {} bytes of {}", rel.display(), bytes.len(), row.size));
        }
        if Some(quickxor(&bytes)) != row.quickxor {
            return Err(format!("{}: the bytes do not match OneDrive's quickXorHash", rel.display()));
        }
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        if read_state(&file).ok().flatten() != Some(State::Hydrated) || read_ctag(&file).ok().flatten() != row.ctag {
            return Err(format!("{}: not hydrated with the row's cTag after the read", rel.display()));
        }
        println!("        {} ({} bytes) verified", rel.display(), row.size);
    }
    Ok(None)
}

/// The free space `statvfs` reports on the filesystem holding `path`, right
/// now — queried fresh on every call, since G2/G3/G4 write real bytes to it
/// as they go.
fn free_bytes(path: &Path) -> Result<u64, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: `c` is a valid NUL-terminated path and `buf` is a plain,
    // properly sized receiver for `statvfs(3)`.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut buf) } != 0 {
        return Err(format!("statvfs({}) failed: {}", path.display(), io::Error::last_os_error()));
    }
    Ok(buf.f_bavail as u64 * buf.f_frsize as u64)
}

/// The largest still-online-only placeholder that is at least 8 MiB (enough
/// room to interrupt a download partway through) and small enough that
/// filling it cannot run the guest's 2 GiB btrfs image out of space: at most
/// a quarter of the free space `statvfs` reports right now, and at most 256
/// MiB either way, so a resume test stays fast. The largest candidate in
/// that window, not the first found, so G4's checkpoint has the most room to
/// land before the download finishes on its own.
///
/// `Ok(None)` — not an error — when nothing in the tree fits the window
/// right now; G3/G4 report that as SKIP. Real files here can be far bigger
/// than a 2 GiB test image (a multi-gigabyte file chosen by size alone once
/// overran the image and both scenarios failed with `ENOSPC`), so the cap is
/// load-bearing,
/// not a nicety. `max_bytes` (`--graph-max-bytes`) folds into it too: the
/// smallest of 256 MiB, a quarter of the free space, and `max_bytes` wins.
fn resumable_placeholder<'a>(folder: &Path, rows: &'a [(Row, PathBuf)], max_bytes: u64) -> Result<Option<&'a (Row, PathBuf)>, String> {
    let free = free_bytes(folder)?;
    let cap = (free / 4).min(256 * MIB).min(max_bytes);
    if cap < 8 * MIB {
        return Ok(None);
    }
    Ok(rows
        .iter()
        .filter(|(r, rel)| {
            r.kind == Kind::File
                && (8 * MIB..=cap).contains(&r.size)
                && std::fs::File::open(folder.join(rel)).ok().and_then(|f| read_state(&f).ok().flatten()) == Some(State::OnlineOnly)
        })
        .max_by_key(|(r, _)| r.size))
}

fn g3(
    service: &Arc<SyncService>,
    graph: &Arc<dyn ContentSource>,
    folder: &Path,
    rows: &[(Row, PathBuf)],
    sticky: bool,
    max_bytes: u64,
) -> Result<Option<String>, String> {
    let Some((row, rel)) = resumable_placeholder(folder, rows, max_bytes)? else {
        return Ok(Some(format!(
            "no online-only file between 8 MiB and a quarter of the free space (capped at 256 MiB and at --graph-max-bytes={max_bytes})"
        )));
    };
    // Partway through whatever was chosen, not a fixed absolute offset — the
    // window above can hand back anything from 8 MiB up to 256 MiB.
    let break_at = (row.size / 2).max(1);
    let watched = Watched::new(Arc::clone(graph), Some(break_at), sticky);
    service.replace_content_source(watched.clone());
    let bytes = read_in_child(&folder.join(rel))?;
    service.replace_content_source(Arc::clone(graph));
    if bytes.len() as u64 != row.size || Some(quickxor(&bytes)) != row.quickxor {
        return Err(format!("{}: not the whole, verified file", rel.display()));
    }
    let froms = watched.froms();
    if froms != vec![0, break_at] {
        return Err(format!("{}: fetches started at {froms:?}, expected [0, {break_at}]", rel.display()));
    }
    Ok(None)
}

async fn g4(
    service: &Arc<SyncService>,
    graph: &Arc<dyn ContentSource>,
    folder: &Path,
    rows: &[(Row, PathBuf)],
    max_bytes: u64,
) -> Result<Option<String>, String> {
    let Some((row, rel)) = resumable_placeholder(folder, rows, max_bytes)? else {
        return Ok(Some(format!(
            "no online-only file between 8 MiB and a quarter of the free space (capped at 256 MiB and at --graph-max-bytes={max_bytes})"
        )));
    };
    // A quarter of the way in, not a fixed absolute offset — scaled the same
    // way as G3's break point, and well short of the whole file so there is
    // still a real resume left to do after the abort below.
    let checkpoint_at = (row.size / 4).max(1);
    let path = folder.join(rel);
    let filling = {
        let service = Arc::clone(service);
        let path = path.clone();
        tokio::spawn(async move { service.hydrate_now(&path).await })
    };
    let deadline = Instant::now() + Duration::from_secs(600);
    let checkpoint = loop {
        let progress = std::fs::File::open(&path).ok().and_then(|f| read_progress(&f).ok().flatten());
        if let Some(p) = progress.filter(|p| p.bytes >= checkpoint_at) {
            break p.bytes;
        }
        if Instant::now() > deadline {
            return Err(format!("no {checkpoint_at}-byte checkpoint within ten minutes"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // The daemon dies mid-download: the fill is dropped where it stands.
    filling.abort();
    let _ = filling.await;
    // What the next start runs first: recovery, on the same helper link.
    let link = service.link().ok_or("no helper link")?;
    let sync_root = service.root().ok_or("no root")?;
    root::recover(&Clearance::Link(link), &sync_root, &service.locks()).await.map_err(|e| e.to_string())?;
    let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let kept = read_progress(&file).ok().flatten().map(|p| p.bytes);
    if read_state(&file).ok().flatten() != Some(State::OnlineOnly) || kept.is_none() {
        return Err(format!("after recovery: {:?} with checkpoint {kept:?}", read_state(&file).ok().flatten()));
    }
    let watched = Watched::new(Arc::clone(graph), None, false);
    service.replace_content_source(watched.clone());
    let bytes = read_in_child(&path)?;
    service.replace_content_source(Arc::clone(graph));
    if bytes.len() as u64 != row.size || Some(quickxor(&bytes)) != row.quickxor {
        return Err("the resumed file is not whole and verified".into());
    }
    let first = watched.froms().first().copied();
    if first != kept {
        return Err(format!("the next download started at {first:?}, the checkpoint was {kept:?} (last seen {checkpoint})"));
    }
    Ok(None)
}
