use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::placeholder::{
    create_placeholder, State,
};
use konedrive_proto::SOCKET_PATH;
use konedrived::config::{ConfigStore, Paths};
use konedrived::helper::{HelperError, HelperLink};
use konedrived::sync::{testing, Persist, SyncError, SyncService};

use crate::harness::{Checks, Ctx, Reader, count_in_log, dir_mark_present, ignore_mark_present};
use crate::{FILESYSTEMS, ROOTS_FILE, statfs_type};
use crate::punch_rule::present;
use crate::races::{all_zeros, got};

/// A fresh, empty folder beside the suite root, on the filesystem under test:
/// somewhere a scenario can register, fill and forget a root of its own
/// without touching the one every other scenario shares.
pub(crate) fn scenario_folder(ctx: &Ctx, name: &str) -> Result<PathBuf, String> {
    let folder = ctx.root.parent().ok_or("the suite root has no parent")?.join(name);
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir(&folder).map_err(|e| format!("cannot create {folder:?}: {e}"))?;
    Ok(folder)
}

/// A 64 KiB placeholder `name` in `folder`, filled by an open from another
/// process — so through the helper, which then ignore-marks it. Returns the
/// path, its inode and the payload. Fails unless the ignore mark is really
/// there afterwards, since every scenario using this is about that mark.
pub(crate) fn hydrated_through_open(
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
pub(crate) fn outcome<T>(result: &Result<T, SyncError>) -> String {
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
pub(crate) fn forget_without_link_refused(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "forget-offline")?;
    let link = ctx.link()?;
    let service = testing::service(Some(link.clone()), None, None);
    let result = forget_without_link_steps(ctx, checks, &service, &folder);

    // Whatever happened, the helper must not keep this folder: forgotten
    // through the link, under whichever registration the daemon ended with.
    service.hub().set_link(Some(link.clone()));
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
pub(crate) fn release_at_the_helper(ctx: &Ctx, link: &HelperLink, folder: &Path) {
    if let Ok(Some(id)) = xattr::get(folder, "user.konedrive.root") {
        if let Ok(id) = String::from_utf8(id) {
            let _ = ctx.runtime.block_on(link.unregister_root(&id));
        }
    }
}

/// Where a service persists its folder since a daemon has several accounts:
/// the one account of `dir/config.toml` — added when the file has none — in a
/// store opened from the file, as each start of the daemon opens it.
pub(crate) async fn one_account(dir: &Path) -> Result<Persist, String> {
    let store = ConfigStore::open(&Paths::in_dir(dir), async { false }).await;
    let account = match store.snapshot().accounts.first() {
        Some(account) => account.id.clone(),
        None => {
            store
                .add_account("Personal")
                .map_err(|e| format!("cannot add an account to {}: {e}", store.file().display()))?
                .id
        }
    };
    Ok(Persist { store: Arc::new(store), account })
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

    // The state the hub's supervisor leaves the service in while the helper is
    // away, and `main.rs` starts it in.
    service.hub().set_link(None);
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
/// its ignore marks. The restart here is a second `SyncService` on a store
/// opened again from the same `config.toml`, with no link, which is exactly
/// what `main.rs` builds before the supervisor's first connect. The first
/// registration is made before `resume` has run at all, which is the order a
/// D-Bus-activated first call can arrive in (zbus claims the name before
/// `main` gets to `resume`); the second after it.
pub(crate) fn pending_root_not_downgraded(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "restarted")?;
    let config_dir = PathBuf::from(format!("/run/konedrive-scenario-config-{}", ctx.fs));
    let _ = std::fs::remove_dir_all(&config_dir);
    let link = ctx.link()?;
    let mut restarted = None;
    let result = pending_root_steps(ctx, checks, &link, &mut restarted, &config_dir, &folder);

    // The folder is forgotten through the helper, under whatever the
    // restarted daemon ended up holding it as.
    if let Some(restarted) = restarted {
        restarted.hub().set_link(Some(link.clone()));
        if restarted.root().is_some() {
            let _ = ctx.runtime.block_on(restarted.unregister_root());
        }
    }
    release_at_the_helper(ctx, &link, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    let _ = std::fs::remove_dir_all(&config_dir);
    result
}

fn pending_root_steps(
    ctx: &Ctx,
    checks: &mut Checks,
    link: &HelperLink,
    restarted: &mut Option<Arc<SyncService>>,
    config_dir: &Path,
    folder: &Path,
) -> Result<(), String> {
    let pid = ctx.helper_pid();
    let mut trace: Vec<String> = Vec::new();

    {
        // The daemon before the restart: registers the folder, and then
        // simply stops — no Forget, as with a logout or a crash.
        let persist = ctx.runtime.block_on(one_account(config_dir))?;
        let before = testing::service(Some(link.clone()), None, Some(persist));
        ctx.runtime
            .block_on(before.register_root(folder))
            .map_err(|e| format!("cannot register {folder:?} with interception: {e}"))?;
    }
    let (path, ino, payload) = hydrated_through_open(ctx, folder, "kept.bin", "ITEM_RESTARTED")?;
    trace.push("registered with interception, hydrated, ignore mark present".into());

    // The restart: `config.toml` read again, and no link yet.
    let persist = ctx.runtime.block_on(one_account(config_dir))?;
    let restarted = restarted.insert(testing::service(None, None, Some(persist)));
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
pub(crate) fn no_interception_forget_is_local(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "unintercepted-forget")?;
    let service = testing::service(Some(ctx.link()?), None, None);
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
pub(crate) fn no_interception_populate_marks_nothing(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "unintercepted-populate")?;
    let source = scenario_folder(ctx, "unintercepted-populate-source")?;
    let service = testing::service(Some(ctx.link()?), None, None);
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
pub(crate) struct ScratchFs {
    image: PathBuf,
    pub(crate) mount: PathBuf,
}

impl ScratchFs {
    pub(crate) fn create(fs: &'static str, name: &str) -> Result<Self, String> {
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
pub(crate) fn no_interception_with_helper_connected(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let scratch = ScratchFs::create(ctx.fs, "unowned")?;
    let folder = scratch.mount.join("root");
    let source = scratch.mount.join("source");
    let service = testing::service(Some(ctx.link()?), None, None);
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
pub(crate) fn upgraded_when_the_helper_starts(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = scenario_folder(ctx, "upgraded")?;
    let source = scenario_folder(ctx, "upgraded-source")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot build a runtime: {e}"))?;
    let service = testing::service(None, None, None);
    let result = upgraded_steps(ctx, checks, &runtime, &service, &folder, &source);

    // Forgotten through the helper, under whatever the daemon ended up
    // holding it as; then this daemon's connection is closed, so that
    // hydrations go back to the suite's own daemon.
    if service.root().is_some() && service.link().is_some() {
        let _ = runtime.block_on(service.unregister_root());
    }
    service.hub().set_link(None);
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
    let supervisor = runtime.spawn(konedrived::helper::hub::supervise(
        Arc::clone(service.hub()),
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

/// Quality findings `HE2`: the helper registers a root only under a root id,
/// and an id registered again onto another directory takes the marks off
/// the directory it had — unless the two overlap, which is refused with
/// every mark left where it was.
///
/// The helper stored whatever string `RegisterRoot` carried, and a
/// re-registration onto another inode dropped the old entry and walked only
/// the new directory: the old tree kept every mark with no registration
/// behind it, so its placeholders were intercepted for a daemon that no
/// longer knew them (`EIO`) until the helper restarted.
pub(crate) fn displaced_root_is_unmarked(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let first = scenario_folder(ctx, "displaced-first")?;
    let second = scenario_folder(ctx, "displaced-second")?;
    let link = ctx.link()?;
    let root_id = "0e1d2c3b-4a59-4687-9675-646973706c61";
    let result = displaced_root_steps(ctx, &link, root_id, &first, &second);
    let _ = ctx.runtime.block_on(link.unregister_root(root_id));
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
    result
}

fn displaced_root_steps(
    ctx: &Ctx,
    link: &HelperLink,
    root_id: &str,
    first: &Path,
    second: &Path,
) -> Result<(), String> {
    // Through the link directly: the trees are there before the walks.
    std::fs::create_dir_all(first.join("a")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(second.join("b")).map_err(|e| e.to_string())?;
    let below_first = ctx.ino_of(&first.join("a"))?;
    let below_second = ctx.ino_of(&second.join("b"))?;
    let one = File::open(first).map_err(|e| e.to_string())?;
    let two = File::open(second).map_err(|e| e.to_string())?;

    match ctx.runtime.block_on(link.register_root(&one, "not-a-root-id")) {
        Err(HelperError::Refused(errno)) if errno == libc::EINVAL => {}
        other => return Err(format!("RegisterRoot under \"not-a-root-id\" → {other:?}, not EINVAL")),
    }
    if dir_mark_present(ctx.helper_pid(), below_first) {
        return Err("a registration that was refused marked the tree".into());
    }

    ctx.runtime
        .block_on(link.register_root(&one, root_id))
        .map_err(|e| format!("cannot register the first directory: {e}"))?;
    if !dir_mark_present(ctx.helper_pid(), below_first) {
        return Err("the first directory was registered and not marked; nothing is tested".into());
    }
    ctx.runtime
        .block_on(link.register_root(&two, root_id))
        .map_err(|e| format!("cannot register the second directory under the same id: {e}"))?;

    let stored = std::fs::read_to_string(ROOTS_FILE).unwrap_or_default();
    let names = |folder: &Path| stored.contains(&format!("\"{}\"", folder.display()));
    let (old_marked, new_marked) = (
        dir_mark_present(ctx.helper_pid(), below_first),
        dir_mark_present(ctx.helper_pid(), below_second),
    );
    println!(
        "    the id registered onto another directory → roots.json names the old: {}, the new: \
         {}; directory mark on the old tree: {old_marked}, on the new: {new_marked}",
        names(first),
        names(second)
    );
    if names(first) || !names(second) {
        return Err("roots.json does not name the new directory alone".into());
    }
    if !new_marked {
        return Err("the directory the id now names was not marked".into());
    }
    if old_marked {
        return Err(
            "the directory the id named before kept its marks, with no registration behind them"
                .into(),
        );
    }

    // The id onto a directory inside the one it names: unmarking the old
    // tree would take the marks off the new one until its walk — a window
    // in which a placeholder there reads zeros. Refused, and nothing moves.
    let inside = File::open(second.join("b")).map_err(|e| e.to_string())?;
    match ctx.runtime.block_on(link.register_root(&inside, root_id)) {
        Err(HelperError::Refused(errno)) if errno == libc::EINVAL => {}
        other => {
            return Err(format!(
                "the id registered onto a directory inside its own → {other:?}, not EINVAL"
            ))
        }
    }
    let stored = std::fs::read_to_string(ROOTS_FILE).unwrap_or_default();
    let still_named = stored.contains(&format!("\"{}\"", second.display()));
    let root_marked = dir_mark_present(ctx.helper_pid(), ctx.ino_of(second)?);
    let below_marked = dir_mark_present(ctx.helper_pid(), below_second);
    println!(
        "    the id registered onto a directory inside its own → EINVAL; roots.json still names \
         the root: {still_named}; directory mark on the root: {root_marked}, below it: \
         {below_marked}"
    );
    if !still_named || !root_marked || !below_marked {
        return Err("a refused move of the id changed the registration or dropped a mark".into());
    }
    Ok(())
}
