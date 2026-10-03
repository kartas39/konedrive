use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::placeholder::{
    create_placeholder, write_state, State,
};
use konedrived::hydration::source::ContentSource;
use konedrived::sync::SyncService;
use konedrived::folder::locks::InodeKey;

use crate::harness::{
    Checks, Ctx, Reader, THROTTLE_SETTLE, UNOPENABLE, ignore_mark_present, occurrences_in_log,
};
use crate::punch_rule::present;
use crate::registration::{hydrated_through_open, release_at_the_helper, scenario_folder};

// ---------------------------------------------------------------------------
// Probes C1, C2, I1, I2, kept as regression scenarios
// ---------------------------------------------------------------------------

/// Whether `content` is `len` bytes of nothing but zeros: what an opener let
/// through onto an unfilled placeholder reads.
pub(crate) fn all_zeros(content: &[u8], len: usize) -> bool {
    content.len() == len && content.iter().all(|&b| b == 0)
}

/// What a reader got, for a trace line.
pub(crate) fn got(result: &Result<Vec<u8>, i32>, payload: &[u8]) -> String {
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
pub(crate) fn stale_request_after_direct_fill(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
            konedrived::hydration::source::hydrate(file.into(), source.as_ref()).await
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
pub(crate) fn inflight_across_forget(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn carried_in_ignore_mark(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn late_ignore_mark(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn leased_file_does_not_stall_others(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
