use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_dbus::testing::TestBus;
use konedrive_proto::SOCKET_PATH;
use konedrived::daemon::manager::{Account, Options};
use konedrived::config::Paths;
use konedrive_graph::oauth::Endpoints;
use konedrived::account::testing::MemoryWallet;
use konedrived::account::state::SignInState;
use konedrived::desktop::baloo::Baloo;
use konedrived::helper::hub;

use crate::harness::{Checks, Ctx, Reader, dir_mark_present};
use crate::ROOTS_FILE;
use crate::registration::{release_at_the_helper, scenario_folder};

// ---------------------------------------------------------------------------
// several accounts, one helper link (multiple-accounts design, test 15)
// ---------------------------------------------------------------------------

/// The files of each account's source in [`two_accounts_one_link`], and their sizes. Both
/// accounts have all three, so the placeholders in both folders carry the same item ids.
const ACCOUNT_FILES: [(&str, usize); 3] = [("doc.bin", 64 * 1024), ("sub/notes.bin", 16 * 1024), ("later.bin", 32 * 1024)];

/// One account of [`two_accounts_one_link`]: its folder, and a source directory holding
/// [`ACCOUNT_FILES`] with bytes of its own.
struct AccountSide {
    label: &'static str,
    account: Arc<Account>,
    folder: PathBuf,
    source: PathBuf,
    files: Vec<(&'static str, Vec<u8>)>,
}

impl AccountSide {
    /// Every byte differs from the other account's at the same offset: `seed` plus the
    /// offset modulo 251.
    fn new(base: &Path, label: &'static str, seed: u8, account: Arc<Account>) -> Result<Self, String> {
        let folder = base.join(label);
        let source = base.join(format!("{label}-source"));
        std::fs::create_dir(&folder).map_err(|e| format!("cannot create {folder:?}: {e}"))?;
        let mut files = Vec::new();
        for (name, len) in ACCOUNT_FILES {
            let bytes: Vec<u8> = (0..len).map(|i| seed.wrapping_add((i % 251) as u8)).collect();
            let path = source.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {path:?}: {e}"))?;
            files.push((name, bytes));
        }
        Ok(Self { label, account, folder, source, files })
    }

    fn bytes(&self, name: &str) -> &[u8] {
        self.files.iter().find(|(n, _)| *n == name).map(|(_, b)| b.as_slice()).unwrap_or_default()
    }

    /// Whether the helper holds a directory mark on the folder, and on its `sub/`.
    fn marked(&self, ctx: &Ctx) -> Result<(bool, bool), String> {
        let pid = ctx.helper_pid();
        Ok((
            dir_mark_present(pid, ctx.ino_of(&self.folder)?),
            dir_mark_present(pid, ctx.ino_of(&self.folder.join("sub"))?),
        ))
    }
}

/// Whose bytes a reader of `name` got, for a trace line.
fn whose(got: &Result<Vec<u8>, i32>, name: &str, sides: &[AccountSide]) -> String {
    match got {
        Err(errno) => format!("errno {errno}"),
        Ok(content) => match sides.iter().find(|side| side.bytes(name) == content.as_slice()) {
            Some(side) => format!("{}'s bytes", side.label),
            None if content.iter().all(|b| *b == 0) => format!("{} zero bytes", content.len()),
            None => format!("{} bytes of neither account", content.len()),
        },
    }
}

/// Multiple accounts, design test 15: one daemon — the account manager `main.rs`
/// starts, on a private bus — with two accounts on one uid, each with an intercepted
/// folder on this filesystem filled from a local source of its own, and one helper link
/// for both, the hub's. The two sources hold files of the same names, so the placeholders
/// in both folders carry the same item ids: only the hub's router — whose folder the file
/// is in, proved by its inode — decides whose bytes an open gets. Then account A is
/// removed: its folder is forgotten through the same link, and B's stays intercepted — a
/// file of B's that nobody opened yet is still filled, with B's bytes, and no file of A's
/// gets them.
pub(crate) fn two_accounts_one_link(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let base = scenario_folder(ctx, "accounts")?;
    let config_dir = PathBuf::from(format!("/run/konedrive-scenario-accounts-{}", ctx.fs));
    let _ = std::fs::remove_dir_all(&config_dir);
    // A runtime of its own, as in `upgraded_when_the_helper_starts`: shut down at the end,
    // it takes this daemon's helper connection with it — the fill loop the hub's supervisor
    // spawns holds the link for as long as it runs — and hydrations go back to the suite's
    // own daemon.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot build a runtime: {e}"))?;
    // `TestBus::start` panics when there is no `dbus-daemon`, and `builder` when it printed
    // no address; one scenario's failure must not end the suite.
    let bus = std::panic::catch_unwind(TestBus::start)
        .ok()
        .filter(|bus| !bus.address().is_empty())
        .ok_or("cannot start a private dbus-daemon for the account manager")?;
    let options = Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(MemoryWallet::default()),
        sign_in_timeout: Duration::from_secs(5),
        baloo: Baloo::disabled,
        thumbnails: None,
        drive: konedrived::daemon::manager::no_drive(),
        bus: Arc::new(konedrived::dbus::export::OnBus),
    };
    let daemon = runtime
        .block_on(konedrived::daemon::startup::start(bus.builder(), Paths::in_dir(&config_dir), options))
        .map_err(|e| format!("cannot start the daemon's accounts: {e:#}"))?;
    let result = two_accounts_steps(ctx, checks, &runtime, &daemon, &base);

    // Every folder still held is forgotten through the helper, then this daemon's
    // connection is closed.
    let hub = Arc::clone(daemon.manager.hub());
    for account in daemon.manager.accounts() {
        if account.sync.root().is_some() && hub.link().is_some() {
            let _ = runtime.block_on(account.sync.unregister_root());
        }
    }
    hub.set_link(None);
    drop(hub);
    {
        let _inside = runtime.enter();
        drop(daemon);
    }
    runtime.shutdown_timeout(Duration::from_secs(5));
    drop(bus);
    if !ctx.helper_alive() || !ctx.daemon_connected() {
        let _ = ctx.restart_helper();
    }
    if let Ok(link) = ctx.link() {
        for label in ["A", "B"] {
            release_at_the_helper(ctx, &link, &base.join(label));
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&config_dir);
    result
}

fn two_accounts_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    runtime: &tokio::runtime::Runtime,
    daemon: &konedrived::daemon::startup::Daemon,
    base: &Path,
) -> Result<(), String> {
    let manager = &daemon.manager;
    let fetches = ctx.fetches();
    let mut trace: Vec<String> = Vec::new();

    // Two accounts, as `Accounts.Add` makes them, each signed in by hand: nothing here
    // reaches Microsoft, and with no drive (`Options::drive` gives none) a folder registered while
    // signed in is a local one — intercepted, and filled from a directory.
    let mut sides = Vec::new();
    for (label, seed) in [("A", 0xA1), ("B", 0xB2)] {
        let account = runtime
            .block_on(manager.add(label, &daemon.connection))
            .map_err(|e| format!("cannot add the account {label}: {e}"))?;
        account.account.state().update(|s| s.state = SignInState::SignedIn);
        sides.push(AccountSide::new(base, label, seed, account)?);
    }

    // The hub's supervisor, as `main.rs` spawns it. Its connection is this uid's newest
    // from now on, so the helper sends it every open of this uid's files.
    runtime.spawn(hub::supervise(Arc::clone(manager.hub()), PathBuf::from(SOCKET_PATH), Duration::from_millis(50)));
    let deadline = Instant::now() + Duration::from_secs(30);
    while manager.hub().link().is_none() {
        if Instant::now() > deadline {
            return Err("the hub's supervisor never connected to the helper".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    for side in &sides {
        let sync = &side.account.sync;
        runtime
            .block_on(sync.register_root(&side.folder))
            .map_err(|e| format!("cannot register {}'s folder with interception: {e}", side.label))?;
        let placed = runtime
            .block_on(sync.populate_from_directory(&side.source))
            .map_err(|e| format!("cannot populate {}'s folder: {e}", side.label))?;
        trace.push(format!("{}: registered, {placed} placeholder(s), RootState {}", side.label, sync.root_state()));
    }
    let marked: Vec<(bool, bool)> = sides.iter().map(|side| side.marked(ctx)).collect::<Result<_, _>>()?;
    trace.push(format!("directory marks (folder, sub/): A {:?}, B {:?}", marked[0], marked[1]));

    // Two files in each folder, one reader each, all at once and from other processes.
    // The names, and so the item ids, are the same in both folders, which are on one
    // filesystem: each open is its account's only by the folder its file is in.
    let mut readers = Vec::new();
    for side in &sides {
        for name in ["doc.bin", "sub/notes.bin"] {
            readers.push((side, name, Reader::start(&ctx.exe, &side.folder.join(name))?));
        }
    }
    let mut wrong = Vec::new();
    for (side, name, reader) in &readers {
        let got = reader.get(Duration::from_secs(60))?;
        let said = whose(&got, name, &sides);
        trace.push(format!("{}/{name}: {said}", side.label));
        if got.as_deref() != Ok(side.bytes(name)) {
            wrong.push(format!("{}/{name} got {said}", side.label));
        }
    }

    // Account A removed, as `Accounts.Remove` removes it.
    let (a, b) = (&sides[0], &sides[1]);
    let root_id = |side: &AccountSide| side.account.sync.root().map(|r| r.root_id).unwrap_or_default();
    let (a_root, b_root) = (root_id(a), root_id(b));
    let removed = runtime.block_on(manager.remove(&a.account.path.as_ref(), &daemon.connection));
    let roots = std::fs::read_to_string(ROOTS_FILE).unwrap_or_default();
    let (a_marked, b_marked) = (a.marked(ctx)?, b.marked(ctx)?);
    let b_state = b.account.sync.root_state();
    trace.push(format!(
        "Remove of A → {}; {} account(s) left; roots.json names A's root: {}, B's: {}; directory \
         marks A {a_marked:?}, B {b_marked:?}; B's RootState {b_state}",
        removed.as_ref().map_or_else(|e| e.to_string(), |()| "Ok".into()),
        manager.accounts().len(),
        roots.contains(&a_root),
        roots.contains(&b_root),
    ));

    // B's folder is still intercepted: a file of B's that nobody opened yet is filled, with
    // B's bytes. A's folder is not, and none of its files may get B's bytes: B's is now the
    // only folder on this filesystem, so an open of A's file intercepted after all would go
    // to B by its filesystem alone.
    let b_later = Reader::start(&ctx.exe, &b.folder.join("later.bin"))?.get(Duration::from_secs(60))?;
    let a_later = Reader::start(&ctx.exe, &a.folder.join("later.bin"))?.get(Duration::from_secs(60))?;
    let fetched = ctx.fetches() - fetches;
    trace.push(format!(
        "after it, B/later.bin: {}, A/later.bin: {}; the suite's own source was asked {fetched} time(s)",
        whose(&b_later, "later.bin", &sides),
        whose(&a_later, "later.bin", &sides),
    ));
    checks.note(ctx.fs, "two accounts", &trace.join("; "));

    let trace = trace.join("; ");
    if marked.iter().any(|(folder, sub)| !folder || !sub) {
        return Err(format!("{trace}. Both folders must be intercepted, down to their subdirectories"));
    }
    if !wrong.is_empty() {
        return Err(format!("{trace}. Each open must be filled from its own account's source: {}", wrong.join(", ")));
    }
    if fetched > 0 {
        return Err(format!("{trace}. The opens must go to the hub's link, not to the suite's daemon"));
    }
    if removed.is_err() || manager.accounts().len() != 1 || roots.contains(&a_root) || a_marked != (false, false) {
        return Err(format!("{trace}. Removing A must forget its folder at the helper"));
    }
    if !roots.contains(&b_root) || b_marked != (true, true) || b_state != "ready" {
        return Err(format!("{trace}. Removing A must leave B's folder registered and intercepted"));
    }
    if b_later.as_deref() != Ok(b.bytes("later.bin")) {
        return Err(format!("{trace}. After A's removal, an open in B's folder must still be filled from B's source"));
    }
    if a_later.as_deref() == Ok(b.bytes("later.bin")) {
        return Err(format!("{trace}. A file of the removed account's folder got the other account's bytes"));
    }
    Ok(())
}
