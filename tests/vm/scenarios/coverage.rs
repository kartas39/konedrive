use std::fs::File;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use konedrive_fs::placeholder::State;

use crate::harness::{Checks, Ctx, dir_mark_present};

pub(crate) fn new_directory_covered(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let dir = ctx.root.join("created-later");
    let _ = std::fs::remove_dir_all(&dir);
    // Created outside the daemon, the way a user's `mkdir` would.
    Command::new("mkdir")
        .arg(&dir)
        .status()
        .map_err(|e| format!("cannot run mkdir: {e}"))?;
    // The daemon has no live directory watcher today: it marks a directory
    // when it creates one (`populate_walk`, invariant M1) and the helper
    // covers whatever exists at startup. A directory somebody else created
    // stays uncovered until one of those two happens, which is what this
    // records.
    let ino = ctx.ino_of(&dir)?;
    if dir_mark_present(ctx.helper_pid(), ino) {
        checks.note(ctx.fs, "new directory", "was covered without anyone asking");
    } else {
        checks.note(
            ctx.fs,
            "new directory",
            "a directory created behind the daemon's back is NOT covered until the daemon marks \
             it or the helper restarts — there is no live watcher",
        );
    }

    let dirfile = File::open(&dir).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    ctx.runtime
        .block_on(link.mark_dir(&dirfile))
        .map_err(|e| format!("MarkDir on a new directory failed: {e}"))?;
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("MarkDir acknowledged success but fdinfo shows no mark".into());
    }

    let path = ctx.place("created-later/inside.bin", "ITEM_NEWDIR", b"INSIDE")?;
    let before = ctx.fetches();
    let content = ctx.read(&path)?;
    if content != b"INSIDE" {
        return Err(format!("the reader got {content:?}"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

pub(crate) fn moved_out_still_covered(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // The control first: a placeholder moved out of the tree with nothing
    // done about it. §1 of the kernel notes expects this to escape the
    // parent's mark entirely, and that is exactly why `MarkFile` exists.
    let loose = ctx.place("loose.bin", "ITEM_LOOSE", b"LOOSE")?;
    let moved_loose = ctx.outside.join("loose.bin");
    let _ = std::fs::remove_file(&moved_loose);
    std::fs::rename(&loose, &moved_loose).map_err(|e| e.to_string())?;
    let outcome = ctx.read(&moved_loose);
    match &outcome {
        Ok(content) if content.iter().all(|b| *b == 0) => checks.note(
            ctx.fs,
            "rename out of the tree",
            "an unmarked file that left the root is NOT intercepted: the open succeeded and read \
             zeros, which is what invariant M4's individual mark exists to prevent",
        ),
        Ok(content) => checks.note(
            ctx.fs,
            "rename out of the tree",
            &format!("the open returned {} bytes of real content", content.len()),
        ),
        Err(e) => checks.note(
            ctx.fs,
            "rename out of the tree",
            &format!("the open was still intercepted: {e}"),
        ),
    }

    // Now the case the design promises: mark the file individually before it
    // leaves, and it stays covered wherever it goes.
    let path = ctx.place("leaving.bin", "ITEM_MOVED", b"MOVED")?;
    let file = File::options().read(true).write(true).open(&path).map_err(|e| e.to_string())?;
    let link = ctx.link()?;
    ctx.runtime
        .block_on(link.mark_file(&file))
        .map_err(|e| format!("MarkFile failed: {e}"))?;
    let ino = ctx.ino_of(&path)?;
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("MarkFile acknowledged success but fdinfo shows no mark on the file".into());
    }
    drop(file);

    let moved = ctx.outside.join("moved.bin");
    let _ = std::fs::remove_file(&moved);
    std::fs::rename(&path, &moved).map_err(|e| e.to_string())?;

    let before = ctx.fetches();
    let content = ctx.read(&moved)?;
    if content != b"MOVED" {
        return Err(format!("the reader got {content:?} after the move"));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

/// Review item 10's remaining two holes, measured rather than assumed. Neither
/// has an assertion attached: what the kernel does here is a fact to record,
/// and the design's own answer to both is invariant M4's individual mark,
/// which the scenario above already proves works.
pub(crate) fn hardlink_and_second_mount(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // A hardlink to a placeholder, in a directory nobody marked.
    let path = ctx.place("linked.bin", "ITEM_LINK", b"LINKED")?;
    let link_path = ctx.outside.join("hardlink.bin");
    let _ = std::fs::remove_file(&link_path);
    std::fs::hard_link(&path, &link_path).map_err(|e| format!("cannot hardlink: {e}"))?;
    let before = ctx.fetches();
    match ctx.read(&link_path) {
        Ok(content) if content == b"LINKED" => checks.note(
            ctx.fs,
            "hardlink in an unmarked directory",
            &format!(
                "the open WAS intercepted and filled ({} fetch(es))",
                ctx.fetches() - before
            ),
        ),
        Ok(content) if content.iter().all(|b| *b == 0) => checks.note(
            ctx.fs,
            "hardlink in an unmarked directory",
            "the open was NOT intercepted: it read zeros, confirming that a parent's mark is \
             consulted during resolution through that parent and is not a property of the inode",
        ),
        Ok(content) => checks.note(
            ctx.fs,
            "hardlink in an unmarked directory",
            &format!("the open returned {} unexpected bytes", content.len()),
        ),
        Err(e) => {
            checks.note(
                ctx.fs,
                "hardlink in an unmarked directory",
                &format!("the open failed: {e}"),
            )
        }
    }
    // Whatever happened, leave the inode in a known state for the next
    // scenario: through the marked name it is either hydrated or not.
    let _ = std::fs::remove_file(&link_path);

    // A second mount of the same filesystem. A bind mount shares the
    // superblock and therefore the inodes, so if a mark were per-mount this
    // is where it would show.
    let second = PathBuf::from(format!("/mnt/{}-second", ctx.fs));
    let _ = std::fs::create_dir_all(&second);
    let status = Command::new("mount")
        .arg("--bind")
        .arg(format!("/mnt/{}", ctx.fs))
        .arg(&second)
        .status()
        .map_err(|e| format!("cannot bind-mount: {e}"))?;
    if !status.success() {
        checks.note(ctx.fs, "second mount", "the bind mount failed; not measured");
        return Ok(());
    }
    let via_second = second.join(
        ctx.root
            .strip_prefix(format!("/mnt/{}", ctx.fs))
            .map_err(|e| e.to_string())?,
    );
    let other = ctx.place("second-mount.bin", "ITEM_SECOND", b"SECOND")?;
    let _ = other;
    let through = via_second.join("second-mount.bin");
    let before = ctx.fetches();
    let outcome = ctx.read(&through);
    match &outcome {
        Ok(content) if content == b"SECOND" => checks.note(
            ctx.fs,
            "second mount",
            &format!(
                "an open through a second mount of the same filesystem IS intercepted and filled \
                 ({} fetch(es))",
                ctx.fetches() - before
            ),
        ),
        Ok(content) if content.iter().all(|b| *b == 0) => checks.note(
            ctx.fs,
            "second mount",
            "an open through a second mount is NOT intercepted: it read zeros",
        ),
        Ok(content) => checks.note(
            ctx.fs,
            "second mount",
            &format!("the open returned {} unexpected bytes", content.len()),
        ),
        Err(e) => checks.note(ctx.fs, "second mount", &format!("the open failed: {e}")),
    }
    let _ = Command::new("umount").arg(&second).status();
    Ok(())
}

pub(crate) fn zero_byte_file(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let path = ctx.place("empty.bin", "ITEM_EMPTY", b"")?;
    let before = ctx.fetches();
    let started = Instant::now();
    let content = ctx.read(&path)?;
    let elapsed = started.elapsed();
    if !content.is_empty() {
        return Err(format!("a zero-byte file returned {} bytes", content.len()));
    }
    if ctx.fetches() != before {
        return Err(format!("a zero-byte file caused {} fetch(es)", ctx.fetches() - before));
    }
    match ctx.state_of(&path)? {
        Some(State::Hydrated) => {}
        other => return Err(format!("a zero-byte placeholder is {other:?}, not hydrated")),
    }
    if elapsed > Duration::from_secs(5) {
        return Err(format!("the open took {elapsed:?}, so it did not go straight through"));
    }
    Ok(())
}
