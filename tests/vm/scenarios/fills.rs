use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;

use konedrive_fs::placeholder::{
    write_state, State, XATTR_STATE,
};
use xattr::FileExt;

use crate::harness::{Checks, Ctx, FAN_OPEN_PERM, Reader, helper_marks, ignore_mark_present};

// ---------------------------------------------------------------------------
// scenarios
// ---------------------------------------------------------------------------

pub(crate) fn open_fills(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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
pub(crate) fn second_open_is_ignored(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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

pub(crate) fn mmap_sees_content(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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

pub(crate) fn copy_sees_content(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
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

pub(crate) fn one_fetch_for_many_openers(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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

/// The helper enrols the waiter in the same step that claims
/// the job (`jobs::enroll`), so a `HydrateDone` cannot arrive before there is
/// anybody to answer. Driven with a daemon that replies as fast as it can, one
/// file at a time, so that the send and the reply race as tightly as the
/// machine allows.
pub(crate) fn instant_reply(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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

pub(crate) fn killed_reader(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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

pub(crate) fn failure_rolls_back(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
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
