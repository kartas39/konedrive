use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::placeholder::create_placeholder;

use crate::harness::{Checks, Ctx, dir_mark_present, ignore_mark_present};

/// The helper's unit runs with
/// `ProtectHome=read-only`, which is a mount namespace in which the tree the
/// sync root lives on is read-only *for the helper*. What has to keep working
/// is the write the **daemon** performs through the descriptor the helper
/// handed it, so the question is whose mount the event fd is opened against.
/// A read-only bind mount in a private namespace reproduces it without
/// systemd.
pub(crate) fn readonly_mount_event_fd(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    // The helper is restarted inside a mount namespace of its own where the
    // whole filesystem under test is read-only.
    let script = format!(
        "mount --bind /mnt/{fs} /mnt/{fs} && mount -o remount,bind,ro /mnt/{fs} && exec \"$@\"",
        fs = ctx.fs
    );
    let binary = ctx.helper.lock().unwrap().binary.clone();
    let log = ctx.helper.lock().unwrap().log.clone();
    ctx.kill_daemon();
    ctx.helper.lock().unwrap().stop();

    let out = File::options().create(true).append(true).open(&log).map_err(|e| e.to_string())?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new("unshare")
        .arg("--mount")
        .arg("--propagation")
        .arg("private")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .arg(&binary)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the helper under unshare: {e}"))?;
    {
        let mut helper = ctx.helper.lock().unwrap();
        helper.child = child;
        helper.await_socket()?;
    }
    ctx.connect_daemon()?;

    let payload = vec![0x5au8; 4096];
    let path = ctx.place("readonly-ns.bin", "ITEM_ROMOUNT", &payload)?;
    let before = ctx.fetches();
    let outcome = ctx.read(&path);
    let result = match &outcome {
        Ok(content) if *content == payload => {
            checks.note(
                ctx.fs,
                "read-only mount",
                "the event fd stayed writable: the kernel opens it against the *opener's* mount, \
                 so ProtectHome=read-only on the helper does not stop a hydration",
            );
            Ok(())
        }
        Ok(content) => Err(format!(
            "the open returned {} bytes, of which {} are wrong",
            content.len(),
            content.iter().zip(&payload).filter(|(a, b)| a != b).count()
        )),
        Err(e) => Err(format!(
            "a hydration failed while the helper's own view of the filesystem was read-only: {e} \
             (this is what ProtectHome=read-only would do in production)"
        )),
    };
    let _ = ctx.fetches() - before;

    // Back to an ordinary helper for everything after this.
    ctx.restart_helper()?;
    result
}

/// Two hazards for the startup walk, in one tree: a symlinked
/// subdirectory (the helper walks as root, so following one is how a user has
/// every directory on the machine marked), and directories being renamed under
/// the walk while it runs.
pub(crate) fn startup_walk_hazards(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let tree = ctx.root.join("walk");
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(tree.join("real/inner")).map_err(|e| e.to_string())?;
    let target = ctx.outside.join("symlink-target");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(target.join("below")).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(tree.join("escape"));
    std::os::unix::fs::symlink(&target, tree.join("escape")).map_err(|e| e.to_string())?;
    // A symlink to a directory *inside* the root, too: following it would
    // double-mark and, worse, is the shape that turns a walk into a loop.
    let _ = std::fs::remove_file(tree.join("loop"));
    std::os::unix::fs::symlink(&tree, tree.join("loop")).map_err(|e| e.to_string())?;

    // A wide tree for the swap to happen in.
    for i in 0..200 {
        std::fs::create_dir_all(tree.join(format!("swap-{i}/child")))
            .map_err(|e| e.to_string())?;
    }

    let renamer = {
        let tree = tree.clone();
        std::thread::spawn(move || {
            // Swap directories underneath the walk for as long as it lasts.
            let deadline = Instant::now() + Duration::from_secs(6);
            let mut n = 0u32;
            while Instant::now() < deadline {
                let i = (n % 200) as usize;
                let from = tree.join(format!("swap-{i}"));
                let to = tree.join(format!("swapped-{i}"));
                let _ = std::fs::rename(&from, &to);
                let _ = std::fs::create_dir(&from);
                let _ = std::fs::create_dir(from.join("child"));
                let _ = std::fs::remove_dir_all(&to);
                n += 1;
            }
            n
        })
    };
    ctx.restart_helper()?;
    let swaps = renamer.join().map_err(|_| "the renaming thread panicked")?;
    checks.note(ctx.fs, "startup walk", &format!("{swaps} directory swaps during the walk"));

    if !ctx.helper_alive() {
        return Err("the helper did not survive a tree changing under its startup walk".into());
    }
    let target_ino = ctx.ino_of(&target)?;
    if dir_mark_present(ctx.helper_pid(), target_ino) {
        return Err(format!(
            "the walk followed a symlink out of the root: ino {target_ino} ({}) is marked",
            target.display()
        ));
    }
    let below_ino = ctx.ino_of(&target.join("below"))?;
    if dir_mark_present(ctx.helper_pid(), below_ino) {
        return Err("the walk descended through a symlink out of the root".into());
    }
    let inner_ino = ctx.ino_of(&tree.join("real/inner"))?;
    if !dir_mark_present(ctx.helper_pid(), inner_ino) {
        return Err("a real subdirectory of the root was not marked by the startup walk".into());
    }

    // And interception still works in the part of the tree that stood still.
    let path = ctx.place("walk/real/inner/after.bin", "ITEM_WALK", b"WALKED")?;
    if ctx.read(&path)? != b"WALKED" {
        return Err("a file under the walked tree was not hydrated".into());
    }
    let _ = std::fs::remove_dir_all(&tree);
    Ok(())
}

/// `readdir`'s `d_type` is only a hint, and some
/// filesystems report `DT_UNKNOWN` for everything; the walk must settle the
/// question with `openat2(O_DIRECTORY)` rather than believe the hint. An ext4
/// image built without the `filetype` feature is a filesystem that really does
/// answer `DT_UNKNOWN`, rather than a constructed one.
pub(crate) fn dt_unknown_walk(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    if ctx.fs != "btrfs" {
        // The image is filesystem-independent; running it once is enough, and
        // it is run under the first suite so a failure is seen early.
        checks.note(ctx.fs, "DT_UNKNOWN", "measured once, under the btrfs suite");
        return Ok(());
    }
    let image = PathBuf::from("/mnt/img/nofiletype.img");
    let mount = PathBuf::from("/mnt/nofiletype");
    let _ = std::fs::create_dir_all(&mount);
    if !image.exists() {
        Command::new("truncate")
            .args(["-s", "64M"])
            .arg(&image)
            .status()
            .map_err(|e| e.to_string())?;
        let out = Command::new("mkfs.ext4")
            .args(["-q", "-F", "-O", "^filetype"])
            .arg(&image)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            checks.note(
                ctx.fs,
                "DT_UNKNOWN",
                &format!(
                    "mkfs.ext4 -O ^filetype is not available here: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            );
            return Ok(());
        }
    }
    let status = Command::new("mount")
        .args(["-o", "loop"])
        .arg(&image)
        .arg(&mount)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("cannot mount the ^filetype image".into());
    }

    let result = (|| -> Result<(), String> {
        let root = mount.join("root");
        std::fs::create_dir_all(root.join("a/b/c")).map_err(|e| e.to_string())?;
        std::fs::write(root.join("a/plain"), b"x").map_err(|e| e.to_string())?;
        // The hint really is DT_UNKNOWN here, or this measures nothing.
        let unknown = nix::dir::Dir::open(
            root.join("a").as_path(),
            nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|e| e.to_string())?
        .iter()
        .flatten()
        .all(|e| e.file_type().is_none());
        if !unknown {
            return Err("the ^filetype image still reports a d_type; nothing is measured here"
                .into());
        }

        // Registered through the helper directly rather than through
        // `root::register_root`: the daemon refuses a folder that is not
        // empty, and the tree has to be there *before* the walk
        // runs or there is nothing to walk.
        let link = ctx.link()?;
        let root_id = "0e1d2c3b-4a59-4687-9675-64742d756e6b";
        let handle = File::open(&root).map_err(|e| e.to_string())?;
        ctx.runtime
            .block_on(link.register_root(&handle, root_id))
            .map_err(|e| format!("cannot register the ^filetype root: {e}"))?;
        let deep = std::fs::metadata(root.join("a/b/c")).map_err(|e| e.to_string())?.ino();
        let marked = dir_mark_present(ctx.helper_pid(), deep);

        // The unregistration walk meets files here only as names whose
        // `d_type` is unknown, so the one way it learns that `a/hydrated` is
        // a file is the `ENOTDIR` from its `O_DIRECTORY` open — and that is
        // where the file's ignore mark has to come off (small round 3, item
        // 1). A file hydrated through an intercepted open carries one.
        let payload = b"UNKNOWN TYPE".to_vec();
        std::fs::write(ctx.source_dir.join("ITEM_DTUNKNOWN"), &payload)
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(root.join("a/hydrated"));
        let parent = File::open(root.join("a")).map_err(|e| e.to_string())?;
        create_placeholder(
            &parent,
            "hydrated",
            "ITEM_DTUNKNOWN",
            payload.len() as u64,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .map_err(|e| format!("cannot create a placeholder on the ^filetype image: {e}"))?;
        drop(parent);
        let hydrated = root.join("a/hydrated");
        let filled = ctx.read(&hydrated);
        let file_ino = ctx.ino_of(&hydrated)?;
        let ignored_before = ignore_mark_present(ctx.helper_pid(), file_ino);

        ctx.runtime
            .block_on(link.unregister_root(root_id))
            .map_err(|e| format!("cannot unregister the ^filetype root: {e}"))?;
        let ignored_after = ignore_mark_present(ctx.helper_pid(), file_ino);
        if !marked {
            return Err(
                "a directory three levels down a DT_UNKNOWN filesystem was not marked".into()
            );
        }
        match filled {
            Ok(content) if content == payload => {}
            Ok(content) => return Err(format!("the file on the ^filetype image read {content:?}")),
            Err(e) => return Err(format!("the file on the ^filetype image did not fill: {e}")),
        }
        if !ignored_before {
            return Err("the hydrated file carries no ignore mark, so nothing is tested".into());
        }
        if ignored_after {
            return Err(format!(
                "ino {file_ino} kept its ignore mark through UnregisterRoot: on a DT_UNKNOWN \
                 filesystem the walk did not clear the ignore mark of a file it only met as \
                 ENOTDIR"
            ));
        }
        Ok(())
    })();

    let _ = Command::new("umount").arg(&mount).status();
    result
}
