use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{
    create_placeholder, read_stamp, write_state, State,
};
use konedrived::helper::Clearance;
use konedrived::folder::root::{self};

use crate::burst::run_burst;
use crate::harness::{Checks, Ctx, Reader, count_in_log};

/// Review item 12, first half. Running out of descriptors is the one failure
/// the event loop survives on purpose: the helper exiting sets
/// every outstanding permission event to *allowed*, which is silent data loss,
/// while a denial is an errno the application can see.
pub(crate) fn emfile_survived(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn worker_panic_contained(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn connection_panic_contained(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn disk_full(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn cross_device_recovery(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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
            .block_on(konedrived::hydration::recovery::recover(&Clearance::Link(link), &sync_root, &ctx.locks))
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
