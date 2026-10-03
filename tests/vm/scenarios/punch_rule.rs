use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::placeholder::{
    create_placeholder, write_state, State,
};
use konedrive_proto::SOCKET_PATH;
use konedrived::sync::{SyncError, SyncService};
use konedrived::folder::locks::InodeKey;

use crate::harness::{
    Checks, Ctx, Reader, THROTTLE_SETTLE, dir_mark_present, ignore_mark_present, occurrences_in_log,
    tail,
};
use crate::HOSTILE_UID;
use crate::races::{all_zeros, got};
use crate::registration::{
    ScratchFs, hydrated_through_open, outcome, release_at_the_helper, scenario_folder,
};

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
pub(crate) fn readonly_mount_open_survived(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn running_executable_open_survived(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn rename_during_registration_walk(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn punch_without_interception_rule(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn clear_ignore_by_ownership(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn recovery_overtaken_by_old_fill(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
    konedrived::hydration::recovery::fault::set_recovery_stall(Duration::ZERO);
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
    konedrived::hydration::recovery::fault::set_recovery_stall(Duration::from_millis(2000));
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
    konedrived::hydration::recovery::fault::set_recovery_stall(Duration::ZERO);

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

pub(crate) fn present(yes: bool) -> &'static str {
    if yes {
        "present"
    } else {
        "absent"
    }
}

pub(crate) fn drop_caches() -> Result<(), String> {
    // SAFETY: a plain sync(2).
    unsafe { libc::sync() };
    std::fs::write("/proc/sys/vm/drop_caches", b"3")
        .map_err(|e| format!("cannot drop caches: {e}"))
}
