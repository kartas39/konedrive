use std::fs::File;
use std::path::Path;
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{
    create_placeholder, State,
};
use konedrived::helper::HelperLink;
use konedrived::hydration::dehydrate::DehydrateError;
use konedrived::sync::{testing, SyncError, SyncService};

use crate::harness::{Checks, Ctx, Holder, dir_mark_present, ignore_mark_present};
use crate::punch_rule::{drop_caches, present};
use crate::registration::outcome;

pub(crate) fn dehydrate_in_use(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let payload = vec![3u8; 8192];
    let path = ctx.place("busy.bin", "ITEM_BUSY", &payload)?;
    let content = ctx.read(&path)?;
    if content != payload {
        return Err("the file was not hydrated first".into());
    }
    let holder = Holder::start(&ctx.exe, &path)?;
    let link = ctx.link()?;
    let sync_root = ctx.sync_root();
    let outcome = ctx.runtime.block_on(konedrived::hydration::dehydrate::dehydrate(&link, &sync_root, &path));
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
pub(crate) fn dehydrate_then_open(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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
        .block_on(konedrived::hydration::dehydrate::dehydrate(&link, &sync_root, &path))
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

/// An evictable mark is designed to vanish, so `ClearIgnore`
/// meets a mark that is not there as a matter of routine; if that ended the
/// connection, every hydration in flight would be denied along with it.
pub(crate) fn clear_ignore_after_reclaim(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
            "the evictable ignore mark is gone after drop_caches, as docs/kernel-behavior-7.2/memory.md §8's argument needs"
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
/// `carried_in_ignore_mark`; both are covered by the registration walk.
///
/// The sequence, driven through the daemon's own
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
pub(crate) fn unregistered_ignore_mark(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let folder = ctx.root.parent().ok_or("the suite root has no parent")?.join("reregistered");
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir(&folder).map_err(|e| format!("cannot create {folder:?}: {e}"))?;
    let link = ctx.link()?;
    let service = testing::service(Some(link.clone()), None, None);
    let result = unregistered_ignore_mark_steps(ctx, checks, &service, &link, &folder);

    // Whatever happened, the helper must not keep this folder: a later
    // scenario would otherwise share the filesystem with a registration
    // nobody here reasons about.
    service.hub().set_link(Some(link));
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
    service.hub().set_link(None);
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
    service.hub().set_link(Some(link.clone()));
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
