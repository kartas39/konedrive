use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::events::{Ev, Labels, drain, info_name, mask_str, read_events};
use crate::{AT_HANDLE_FID, fan};
use crate::root::FILL_FILES;
use crate::sys::{
    cpath, design_group, ename, fan_init, fan_mark, fsid_of, io_err, last_errno, name_to_handle,
    ok_or, open_by_handle, read_sysctl, show_fsid,
};

// ---------------------------------------------------------------------------
// Unprivileged side
// ---------------------------------------------------------------------------

pub(crate) fn child_main(args: &[String]) -> i32 {
    let mode = args.first().map(String::as_str).unwrap_or("");
    let p = |i: usize| PathBuf::from(&args[i]);
    match mode {
        "flags" => child_flags(&p(1), &p(2), &p(3), &p(4), &args[5]),
        "matrix" => child_matrix(&p(1)),
        "fill" => child_fill(&p(1)),
        "overflow" => child_overflow(&p(1)),
        "marklimit" => child_marklimit(&p(1)),
        _ => {
            println!("unknown child mode {mode:?}");
            1
        }
    }
}

fn identity() -> String {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| {
        status
            .lines()
            .find(|l| l.starts_with(k))
            .map(|l| l[k.len()..].split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    };
    format!(
        "Uid {} / Gid {} / Groups [{}] / CapEff {} / CapPrm {}",
        field("Uid:"),
        field("Gid:"),
        field("Groups:"),
        field("CapEff:"),
        field("CapPrm:")
    )
}

fn child_flags(own: &Path, rootonly: &Path, fsroot: &Path, tmpfs: &Path, subvol: &str) -> i32 {
    println!("== the unprivileged child: {}", identity());

    println!("== fanotify_init, unprivileged (event_f_flags O_RDONLY|O_CLOEXEC)");
    let d = fan::DESIGN_INIT;
    let cases: [(&str, u32); 16] = [
        ("FAN_CLASS_NOTIF, no FID reporting", fan::CLASS_NOTIF),
        ("FAN_REPORT_FID", fan::REPORT_FID),
        ("FAN_REPORT_DIR_FID", fan::REPORT_DIR_FID),
        ("FAN_REPORT_DFID_NAME", fan::REPORT_DFID_NAME),
        ("FAN_REPORT_DFID_NAME | FAN_REPORT_FID", fan::REPORT_DFID_NAME | fan::REPORT_FID),
        ("FAN_REPORT_DFID_NAME_TARGET | FAN_NONBLOCK | FAN_CLOEXEC (the design's)", d),
        ("the design's + FAN_UNLIMITED_QUEUE", d | fan::UNLIMITED_QUEUE),
        ("the design's + FAN_UNLIMITED_MARKS", d | fan::UNLIMITED_MARKS),
        ("the design's + FAN_REPORT_TID", d | fan::REPORT_TID),
        ("the design's + FAN_REPORT_PIDFD", d | fan::REPORT_PIDFD),
        ("the design's + FAN_ENABLE_AUDIT", d | fan::ENABLE_AUDIT),
        ("the design's + FAN_REPORT_FD_ERROR", d | fan::REPORT_FD_ERROR),
        ("FAN_REPORT_MNT", fan::REPORT_MNT),
        ("FAN_CLASS_CONTENT | FAN_REPORT_FID", fan::CLASS_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT | FAN_REPORT_FID", fan::CLASS_PRE_CONTENT | fan::REPORT_FID),
        ("FAN_CLASS_PRE_CONTENT", fan::CLASS_PRE_CONTENT),
    ];
    for (label, flags) in cases {
        let r = fan_init(flags, (libc::O_RDONLY | libc::O_CLOEXEC) as u32);
        println!("  {label}: {}", ok_or(r));
    }

    {
        println!("== fanotify_mark, unprivileged, in the design's group");
        let g = design_group();
        let gx = design_group();
        let full = fan::DESIGN_MASK | fan::MODIFY | fan::DELETE_SELF | fan::MOVE_SELF;
        println!(
            "  inode mark, own directory, the design's mask: {}",
            ok_or(fan_mark(&g, fan::MARK_ADD, fan::DESIGN_MASK, own))
        );
        println!(
            "  the same, + FAN_MODIFY | FAN_DELETE_SELF | FAN_MOVE_SELF: {}",
            ok_or(fan_mark(&g, fan::MARK_ADD, full, own))
        );
        println!(
            "  FAN_MARK_MOUNT on own directory: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD | fan::MARK_MOUNT, fan::CREATE, own))
        );
        println!(
            "  FAN_MARK_FILESYSTEM on own directory: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD | fan::MARK_FILESYSTEM, fan::CREATE, own))
        );
        println!(
            "  inode mark on a root-owned 0755 directory ({}): {}",
            fsroot.display(),
            ok_or(fan_mark(&gx, fan::MARK_ADD, fan::DESIGN_MASK, fsroot))
        );
        println!(
            "  inode mark on a root-owned 0700 directory: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD, fan::DESIGN_MASK, rootonly))
        );
        println!(
            "  FAN_OPEN_PERM in a notification group: {}",
            ok_or(fan_mark(&gx, fan::MARK_ADD, fan::OPEN_PERM | fan::EVENT_ON_CHILD, own))
        );
        let gfid = fan_init(fan::REPORT_FID | fan::NONBLOCK, libc::O_RDONLY as u32).unwrap();
        println!(
            "  FAN_RENAME in a FAN_REPORT_FID-only group: {}",
            ok_or(fan_mark(&gfid, fan::MARK_ADD, fan::RENAME, own))
        );
        let gdfid = fan_init(fan::REPORT_DIR_FID | fan::NONBLOCK, libc::O_RDONLY as u32).unwrap();
        println!(
            "  FAN_RENAME in a FAN_REPORT_DIR_FID group (no names): {}",
            ok_or(fan_mark(&gdfid, fan::MARK_ADD, fan::RENAME, own))
        );
        println!(
            "  FAN_CREATE|FAN_ONDIR|FAN_EVENT_ON_CHILD in a FAN_REPORT_FID-only group: {}",
            ok_or(fan_mark(&gfid, fan::MARK_ADD, fan::CREATE | fan::ONDIR | fan::EVENT_ON_CHILD, own))
        );

        println!("== handles, unprivileged");
        let (hdir, mnt_id) = match name_to_handle(own, 0) {
            Ok(x) => x,
            Err(e) => {
                println!("  name_to_handle_at(own directory): {}", ename(e));
                return 1;
            }
        };
        println!("  name_to_handle_at(own directory): OK, {}, mount id {mnt_id}", hdir.show());
        match name_to_handle(own, AT_HANDLE_FID) {
            Ok((h, _)) => println!(
                "  name_to_handle_at(own directory, AT_HANDLE_FID): OK, {} ({} the plain handle)",
                h.show(),
                if h == hdir { "equal to" } else { "DIFFERENT from" }
            ),
            Err(e) => println!("  name_to_handle_at(own directory, AT_HANDLE_FID): {}", ename(e)),
        }
        let _ = read_events(&g);
        let file = own.join("probe-file");
        File::create(&file).unwrap();
        let (hfile, _) = name_to_handle(&file, 0).unwrap();
        let events = drain(&g);
        let statfs_fsid = fsid_of(own);
        println!("  statfs(own directory).f_fsid = {}", show_fsid(statfs_fsid));
        for ev in &events {
            println!(
                "  event {} pid={} (own pid {}), fd={}, {} info record(s):",
                mask_str(ev.mask),
                ev.pid,
                std::process::id(),
                ev.fd,
                ev.recs.len()
            );
            for r in &ev.recs {
                let Some(h) = &r.handle else {
                    println!("    {} (len {})", info_name(r.itype), r.len);
                    continue;
                };
                let against = if r.name.is_some() { &hdir } else { &hfile };
                println!(
                    "    {} fsid {} ({} statfs) handle {} name {:?} — {} name_to_handle_at({})",
                    info_name(r.itype),
                    show_fsid(r.fsid),
                    if r.fsid == statfs_fsid { "==" } else { "!=" },
                    h.show(),
                    r.name.as_deref().unwrap_or("-"),
                    if h == against { "EQUAL to" } else { "DIFFERENT from" },
                    if r.name.is_some() { "the directory" } else { "the file" }
                );
            }
        }
        let dirfd = File::open(own).unwrap();
        println!(
            "  open_by_handle_at(file handle, O_RDONLY), unprivileged: {}",
            ok_or(open_by_handle(dirfd.as_raw_fd(), &hfile, libc::O_RDONLY))
        );
        println!(
            "  open_by_handle_at(directory handle, O_RDONLY|O_DIRECTORY), unprivileged: {}",
            ok_or(open_by_handle(dirfd.as_raw_fd(), &hdir, libc::O_RDONLY | libc::O_DIRECTORY))
        );
        let sub = own.join("sub");
        fs::create_dir(&sub).unwrap();
        let (h1, _) = name_to_handle(&sub, 0).unwrap();
        fs::rename(&sub, own.join("sub-renamed")).unwrap();
        let (h2, _) = name_to_handle(&own.join("sub-renamed"), 0).unwrap();
        println!(
            "  a directory's handle before and after a rename: {}",
            if h1 == h2 { "equal" } else { "DIFFERENT" }
        );
        let _ = read_events(&g);

        println!("== tmpfs ({}), unprivileged", tmpfs.display());
        let gt = design_group();
        let r = fan_mark(&gt, fan::MARK_ADD, fan::DESIGN_MASK, tmpfs);
        println!("  inode mark with the design's group: {}", ok_or(r));
        match name_to_handle(tmpfs, 0) {
            Ok((h, _)) => {
                println!("  name_to_handle_at: OK, {}", h.show());
                if r.is_ok() {
                    File::create(tmpfs.join("f")).unwrap();
                    let fsid = fsid_of(tmpfs);
                    for ev in drain(&gt) {
                        let rec = ev.recs.iter().find(|r| r.name.is_some());
                        println!(
                            "  event {} — DFID {} name_to_handle_at, fsid {} ({} statfs)",
                            mask_str(ev.mask),
                            if rec.and_then(|r| r.handle.as_ref()) == Some(&h) { "EQUAL to" } else { "DIFFERENT from" },
                            rec.map(|r| show_fsid(r.fsid)).unwrap_or_default(),
                            if rec.map(|r| r.fsid) == Some(fsid) { "==" } else { "!=" },
                        );
                    }
                }
            }
            Err(e) => println!("  name_to_handle_at: {}", ename(e)),
        }

        if subvol != "-" {
            let sv = Path::new(subvol);
            println!("== a btrfs subvolume inside the filesystem, unprivileged");
            let gs = design_group();
            println!(
                "  mark a directory on the top-level subvolume: {}",
                ok_or(fan_mark(&gs, fan::MARK_ADD, fan::DESIGN_MASK, own))
            );
            let r = fan_mark(&gs, fan::MARK_ADD, fan::DESIGN_MASK, sv);
            println!("  mark the subvolume's root, in the same group: {}", ok_or(r));
            println!(
                "  statfs f_fsid: top-level {} / subvolume {}",
                show_fsid(fsid_of(own)),
                show_fsid(fsid_of(sv))
            );
            let alone = design_group();
            let r2 = fan_mark(&alone, fan::MARK_ADD, fan::DESIGN_MASK, sv);
            println!("  mark the subvolume's root in a group of its own: {}", ok_or(r2));
            if r2.is_ok() {
                let (h, _) = name_to_handle(sv, 0).unwrap();
                File::create(sv.join("in-own-group")).unwrap();
                for ev in drain(&alone) {
                    let rec = ev.recs.iter().find(|r| r.name.is_some()).unwrap();
                    println!(
                        "  event {} name {:?}: fsid {} ({} statfs(subvolume)); DFID {} name_to_handle_at(subvolume root)",
                        mask_str(ev.mask),
                        rec.name.as_deref().unwrap_or(""),
                        show_fsid(rec.fsid),
                        if rec.fsid == fsid_of(sv) { "==" } else { "!=" },
                        if rec.handle.as_ref() == Some(&h) { "EQUAL to" } else { "not equal to" }
                    );
                }
            }
            if r.is_ok() {
                let (h, _) = name_to_handle(sv, 0).unwrap();
                File::create(sv.join("f")).unwrap();
                File::create(own.join("g")).unwrap();
                for ev in drain(&gs) {
                    let rec = ev.recs.iter().find(|r| r.name.is_some()).unwrap();
                    println!(
                        "  event {} name {:?}: fsid {}; DFID {} name_to_handle_at(subvolume root)",
                        mask_str(ev.mask),
                        rec.name.as_deref().unwrap_or(""),
                        show_fsid(rec.fsid),
                        if rec.handle.as_ref() == Some(&h) { "EQUAL to" } else { "not equal to" }
                    );
                }
            }
        }
    }

    println!("== per-user group limit (max_user_groups = {})", read_sysctl("max_user_groups"));
    let mut held = Vec::new();
    let err = loop {
        match fan_init(fan::DESIGN_INIT, libc::O_RDONLY as u32) {
            Ok(fd) => held.push(fd),
            Err(e) => break Some(e),
        }
        if held.len() >= 4096 {
            break None;
        }
    };
    println!(
        "  {} groups created, then {}",
        held.len(),
        err.map(ename).unwrap_or_else(|| "no refusal up to 4096".into())
    );
    drop(held);
    0
}

/// The matrix of operations, each followed by the events it produced.
struct Matrix {
    group: OwnedFd,
    labels: Labels,
}

impl Matrix {
    fn step(&mut self, name: &str, f: impl FnOnce() -> Result<(), String>) {
        let outcome = f();
        let events = drain(&self.group);
        match outcome {
            Ok(()) => println!("op: {name}"),
            Err(e) => println!("op: {name}  [op failed: {e}]"),
        }
        if events.is_empty() {
            println!("    (no events)");
        }
        for ev in &events {
            println!("    {}", self.labels.fmt(ev));
        }
    }
}

fn io<T>(r: std::io::Result<T>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {}", io_err(&e)))
}

fn write_file(p: &Path, content: &[u8]) -> Result<(), String> {
    io(fs::write(p, content), "write")
}

fn child_matrix(base: &Path) -> i32 {
    let tree = base.join("tree");
    let a = tree.join("A");
    let b = tree.join("B");
    let out = base.join("out");

    // Everything that exists before the marks: created first, so none of it
    // is an event.
    let pre_a = ["doc", "rmme", "chm", "h", "outfile", "xa", "touchme"];
    for n in pre_a {
        fs::write(a.join(n), b"v1").unwrap();
    }
    fs::create_dir(a.join("outdir")).unwrap();
    fs::write(a.join("outdir/inner"), b"x").unwrap();
    fs::write(out.join("in-file"), b"x").unwrap();
    fs::create_dir(out.join("in-dir")).unwrap();
    fs::write(out.join("in-dir/inner"), b"x").unwrap();
    fs::write(out.join("lnk-src"), b"x").unwrap();

    let group = design_group();
    let all = fan::DESIGN_MASK | fan::MODIFY;
    fan_mark(&group, fan::MARK_ADD, all | fan::DELETE_SELF | fan::MOVE_SELF, &tree).unwrap();
    fan_mark(&group, fan::MARK_ADD, all, &a).unwrap();
    fan_mark(&group, fan::MARK_ADD, all, &b).unwrap();
    println!(
        "group: FAN_CLASS_NOTIF|FAN_REPORT_DFID_NAME_TARGET; marks: tree = design mask|MODIFY|DELETE_SELF|MOVE_SELF, \
         A and B = design mask|MODIFY; out is not marked"
    );
    println!("labels: handles from name_to_handle_at taken before any event; #n = an object first seen in an event");

    let mut labels = Labels::new(fsid_of(&tree));
    labels.add_dir("tree", &tree);
    labels.add_dir("A", &a);
    labels.add_dir("B", &b);
    labels.add_dir("out", &out);
    for n in pre_a {
        labels.add_obj(n, &a.join(n));
    }
    labels.add_obj("outdir", &a.join("outdir"));
    labels.add_obj("in-file", &out.join("in-file"));
    labels.add_obj("in-dir", &out.join("in-dir"));
    labels.add_obj("lnk-src", &out.join("lnk-src"));
    let mut m = Matrix { group, labels };
    let pre = drain(&m.group);
    println!("before any operation: {} event(s)", pre.len());

    let j = |p: &Path, n: &str| p.join(n);

    m.step("create a file: open(A/new, O_CREAT|O_EXCL|O_WRONLY), close", || {
        io(fs::OpenOptions::new().write(true).create_new(true).open(j(&a, "new")), "open").map(drop)
    });
    m.step("mkdir A/d1", || io(fs::create_dir(j(&a, "d1")), "mkdir"));
    let mut held: Option<File> = None;
    m.step("open A/new O_WRONLY and write 3 bytes, descriptor still open", || {
        let mut f = io(fs::OpenOptions::new().write(true).open(j(&a, "new")), "open")?;
        io(f.write_all(b"abc"), "write")?;
        held = Some(f);
        Ok(())
    });
    m.step("... then close it", || {
        drop(held.take());
        Ok(())
    });
    m.step("write+close in one go: open A/new O_WRONLY|O_APPEND, write, close", || {
        let mut f = io(fs::OpenOptions::new().append(true).open(j(&a, "new")), "open")?;
        io(f.write_all(b"def"), "write")
    });
    m.step("truncate(A/new, 0) by path", || {
        let c = cpath(&j(&a, "new"));
        if unsafe { libc::truncate(c.as_ptr(), 0) } < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("rename within a directory: A/new -> A/renamed", || {
        io(fs::rename(j(&a, "new"), j(&a, "renamed")), "rename")
    });
    m.step("rename a directory within a directory: A/d1 -> A/d1r", || {
        io(fs::rename(j(&a, "d1"), j(&a, "d1r")), "rename")
    });
    m.step("rename across marked directories: A/renamed -> B/renamed", || {
        io(fs::rename(j(&a, "renamed"), j(&b, "renamed")), "rename")
    });
    m.step("move a file into the tree: out/in-file -> A/in-file", || {
        io(fs::rename(j(&out, "in-file"), j(&a, "in-file")), "rename")
    });
    m.step("move a directory into the tree: out/in-dir -> A/in-dir", || {
        io(fs::rename(j(&out, "in-dir"), j(&a, "in-dir")), "rename")
    });
    m.step("create a file inside the moved-in directory A/in-dir (which nobody marked)", || {
        write_file(&a.join("in-dir/fresh"), b"x")
    });
    m.step("move a file out of the tree: A/outfile -> out/outfile", || {
        io(fs::rename(j(&a, "outfile"), j(&out, "outfile")), "rename")
    });
    m.step("move a directory out of the tree: A/outdir -> out/outdir", || {
        io(fs::rename(j(&a, "outdir"), j(&out, "outdir")), "rename")
    });
    m.step("unlink A/rmme", || io(fs::remove_file(j(&a, "rmme")), "unlink"));
    m.step("rmdir A/d1r", || io(fs::remove_dir(j(&a, "d1r")), "rmdir"));
    m.step("chmod A/chm 0600", || {
        io(fs::set_permissions(j(&a, "chm"), fs::Permissions::from_mode(0o600)), "chmod")
    });
    m.step("chmod a marked directory itself: A 0700", || {
        io(fs::set_permissions(&a, fs::Permissions::from_mode(0o700)), "chmod")
    });
    m.step("chmod an unmarked child directory: A/in-dir 0700", || {
        io(fs::set_permissions(j(&a, "in-dir"), fs::Permissions::from_mode(0o700)), "chmod")
    });
    m.step("touch A/touchme (utimensat, both times to now)", || {
        let c = cpath(&j(&a, "touchme"));
        if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) } < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("setxattr A/xa user.test", || {
        let c = cpath(&j(&a, "xa"));
        let key = CString::new("user.test").unwrap();
        if unsafe { libc::setxattr(c.as_ptr(), key.as_ptr(), b"1".as_ptr().cast(), 1, 0) } < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("safe save: write A/.doc.tmp, close, rename it over A/doc", || {
        write_file(&j(&a, ".doc.tmp"), b"v2")?;
        io(fs::rename(j(&a, ".doc.tmp"), j(&a, "doc")), "rename")
    });
    m.step("backup-rename save: A/doc -> A/doc~, write a new A/doc, close, unlink A/doc~", || {
        io(fs::rename(j(&a, "doc"), j(&a, "doc~")), "rename")?;
        write_file(&j(&a, "doc"), b"v3")?;
        io(fs::remove_file(j(&a, "doc~")), "unlink")
    });
    m.step("hard link within the tree: A/h -> A/h2", || {
        io(fs::hard_link(j(&a, "h"), j(&a, "h2")), "link")
    });
    m.step("hard link into the tree: out/lnk-src -> A/lnk", || {
        io(fs::hard_link(j(&out, "lnk-src"), j(&a, "lnk")), "link")
    });
    m.step("hard link out of the tree: A/h -> out/h-out", || {
        io(fs::hard_link(j(&a, "h"), j(&out, "h-out")), "link")
    });
    m.step("write+close A/h's content through the outside link out/h-out", || {
        let mut f = io(fs::OpenOptions::new().append(true).open(j(&out, "h-out")), "open")?;
        io(f.write_all(b"through the other name"), "write")
    });
    let mut tmp: Option<File> = None;
    m.step("O_TMPFILE in A, write 3 bytes (no name yet)", || {
        let mut f = io(
            fs::OpenOptions::new().write(true).custom_flags(libc::O_TMPFILE).mode(0o644).open(&a),
            "O_TMPFILE",
        )?;
        io(f.write_all(b"tmp"), "write")?;
        tmp = Some(f);
        Ok(())
    });
    m.step("... linkat it in as A/linked", || {
        let f = tmp.as_ref().ok_or("no tmpfile")?;
        let from = CString::new(format!("/proc/self/fd/{}", f.as_raw_fd())).unwrap();
        let to = cpath(&j(&a, "linked"));
        let rc = unsafe {
            libc::linkat(libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), libc::AT_SYMLINK_FOLLOW)
        };
        if rc < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("... then close it", || {
        drop(tmp.take());
        Ok(())
    });
    m.step("renameat2(RENAME_EXCHANGE) A/h2 <-> A/chm", || {
        let x = cpath(&j(&a, "h2"));
        let y = cpath(&j(&a, "chm"));
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, x.as_ptr(), libc::AT_FDCWD, y.as_ptr(), libc::RENAME_EXCHANGE)
        };
        if rc < 0 {
            return Err(ename(last_errno()));
        }
        Ok(())
    });
    m.step("create+write A/thr from a second thread of this process", || {
        let p = j(&a, "thr");
        thread::spawn(move || write_file(&p, b"t")).join().map_err(|_| "thread panicked".to_string())?
    });
    m.step("create+write A/child from a child process (/bin/sh -c 'echo x > A/child')", || {
        let st = io(
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("echo x > '{}'", j(&a, "child").display()))
                .status(),
            "spawn",
        )?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("sh exited {st}"))
        }
    });
    let sh_append = |p: PathBuf| -> Result<(), String> {
        let st = io(
            Command::new("/bin/sh").arg("-c").arg(format!("echo y >> '{}'", p.display())).status(),
            "spawn",
        )?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("sh exited {st}"))
        }
    };
    m.step("A/mixed: this process creates+writes it, then a child process appends, nothing read between", || {
        write_file(&j(&a, "mixed"), b"x")?;
        sh_append(j(&a, "mixed"))
    });
    m.step("A/mixed: a child process appends, then this process appends, nothing read between", || {
        sh_append(j(&a, "mixed"))?;
        let mut f = io(fs::OpenOptions::new().append(true).open(j(&a, "mixed")), "open")?;
        io(f.write_all(b"z"), "write")
    });
    let moved = base.join("tree-moved");
    m.step("rename the marked root: tree -> tree-moved (its parent is not marked)", || {
        io(fs::rename(&tree, &moved), "rename")
    });
    m.labels.move_dir("tree", &moved);
    m.labels.move_dir("A", &moved.join("A"));
    m.labels.move_dir("B", &moved.join("B"));
    m.step("rm -rf tree-moved", || io(fs::remove_dir_all(&moved), "remove_dir_all"));
    thread::sleep(Duration::from_millis(300));
    let late = read_events(&m.group);
    println!("late events, 300 ms later: {}", late.len());
    for ev in &late {
        println!("    {}", m.labels.fmt(ev));
    }
    0
}

fn child_fill(dir: &Path) -> i32 {
    let group = design_group();
    fan_mark(&group, fan::MARK_ADD, fan::DESIGN_MASK | fan::MODIFY, dir).unwrap();
    let mut labels = Labels::new(fsid_of(dir));
    labels.add_dir("F", dir);
    for n in FILL_FILES {
        labels.add_obj(n, &dir.join(n));
    }
    let mut m = Matrix { group, labels };
    for name in &FILL_FILES[..6] {
        let p = dir.join(name);
        m.step(&format!("open {name} O_RDONLY, read it, close (the root group answers the open)"), || {
            let mut f = io(File::open(&p), "open")?;
            let mut v = Vec::new();
            io(f.read_to_end(&mut v), "read")?;
            Ok(())
        });
    }
    let deny = dir.join("denyme");
    let mut note = String::new();
    m.step("open(F/denyme, O_CREAT|O_WRONLY) while the root group denies opens of that name", || {
        let r = fs::OpenOptions::new().write(true).create(true).open(&deny);
        let exists = deny.exists();
        match r {
            Ok(_) => Err(format!("the open succeeded; file exists: {exists}")),
            Err(e) => {
                note = format!("the open failed {}; the file exists afterwards: {exists}", io_err(&e));
                Ok(())
            }
        }
    });
    println!("    ({note})");

    // The same kinds of write, on a descriptor this process opened itself:
    // what the daemon's own writes look like to its own group.
    let own = dir.join("fill-own");
    let f = match fs::OpenOptions::new().read(true).write(true).open(&own) {
        Ok(f) => f,
        Err(e) => {
            println!("open fill-own: {}", io_err(&e));
            return 1;
        }
    };
    let fd = f.as_raw_fd();
    m.step("own descriptor on F/fill-own (O_RDWR): open", || Ok(()));
    m.step("own descriptor: pwrite 5 bytes", || {
        let n = unsafe { libc::pwrite(fd, b"HELLO".as_ptr().cast(), 5, 0) };
        if n < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: ftruncate(16384)", || {
        if unsafe { libc::ftruncate(fd, 16384) } < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: futimens", || {
        let ts = [
            libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
            libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 },
        ];
        if unsafe { libc::futimens(fd, ts.as_ptr()) } < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: fsetxattr user.konedrive.state", || {
        let key = CString::new("user.konedrive.state").unwrap();
        if unsafe { libc::fsetxattr(fd, key.as_ptr(), b"hydrated".as_ptr().cast(), 8, 0) } < 0 {
            Err(ename(last_errno()))
        } else {
            Ok(())
        }
    });
    m.step("own descriptor: fallocate(PUNCH_HOLE|KEEP_SIZE)", || {
        let r = unsafe {
            libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 0, 4096)
        };
        if r < 0 { Err(ename(last_errno())) } else { Ok(()) }
    });
    m.step("own descriptor: close", || {
        drop(f);
        Ok(())
    });
    0
}

fn child_overflow(dir: &Path) -> i32 {
    let max: usize = read_sysctl("max_queued_events").parse().unwrap_or(16384);
    let group = design_group();
    fan_mark(&group, fan::MARK_ADD, fan::CREATE, dir).unwrap();
    let n = max + 100;
    let t0 = Instant::now();
    for i in 0..n {
        File::create(dir.join(format!("f{i}"))).unwrap();
    }
    println!(
        "  max_queued_events = {max}; created {n} files in the marked directory in {:?}, reading nothing meanwhile",
        t0.elapsed()
    );
    let events = read_events(&group);
    let creates = events.iter().filter(|e| e.mask & fan::CREATE != 0).count();
    let overflows: Vec<(usize, &Ev)> =
        events.iter().enumerate().filter(|(_, e)| e.mask & fan::Q_OVERFLOW != 0).collect();
    println!("  read {} events: {creates} FAN_CREATE, {} FAN_Q_OVERFLOW", events.len(), overflows.len());
    for (i, e) in overflows {
        println!(
            "  overflow at position {i} of {}: mask {}, event_len {}, metadata_len {}, fd {}, pid {}, {} info record(s)",
            events.len(),
            mask_str(e.mask),
            e.event_len,
            e.metadata_len,
            e.fd,
            e.pid,
            e.recs.len()
        );
    }
    File::create(dir.join("after")).unwrap();
    let after = drain(&group);
    println!(
        "  after draining, one more create: {} event(s) {}",
        after.len(),
        after.iter().map(|e| mask_str(e.mask)).collect::<Vec<_>>().join(", ")
    );
    println!(
        "  FAN_UNLIMITED_QUEUE unprivileged: {}",
        ok_or(fan_init(fan::DESIGN_INIT | fan::UNLIMITED_QUEUE, libc::O_RDONLY as u32))
    );
    0
}

fn child_marklimit(dir: &Path) -> i32 {
    let limit = read_sysctl("max_user_marks");
    println!("  max_user_marks as the child reads it: {limit}");
    let dirs: Vec<PathBuf> = (0..60).map(|i| dir.join(format!("d{i}"))).collect();
    for d in &dirs {
        fs::create_dir(d).unwrap();
    }
    let g1 = design_group();
    let mut ok1 = 0;
    for d in &dirs[..25] {
        match fan_mark(&g1, fan::MARK_ADD, fan::DESIGN_MASK, d) {
            Ok(()) => ok1 += 1,
            Err(e) => {
                println!("  group 1: mark {} refused {}", ok1 + 1, ename(e));
                break;
            }
        }
    }
    println!("  group 1: {ok1} marks placed");
    let g2 = design_group();
    let mut ok2 = 0;
    let mut failed_at = None;
    for d in &dirs[25..] {
        match fan_mark(&g2, fan::MARK_ADD, fan::DESIGN_MASK, d) {
            Ok(()) => ok2 += 1,
            Err(e) => {
                failed_at = Some((d.clone(), e));
                break;
            }
        }
    }
    match &failed_at {
        Some((_, e)) => println!(
            "  group 2: {ok2} marks placed, the next refused {} — {} marks in total for this uid",
            ename(*e),
            ok1 + ok2
        ),
        None => println!("  group 2: {ok2} marks placed, no refusal"),
    }
    let again_same = fan_mark(&g2, fan::MARK_ADD, fan::DESIGN_MASK | fan::MODIFY, &dirs[25]);
    println!("  adding bits to a directory group 2 already marks, at the limit: {}", ok_or(again_same));
    if let Some((d, _)) = failed_at {
        let removed = fan_mark(&g1, fan::MARK_REMOVE, fan::DESIGN_MASK, &dirs[0]);
        println!("  group 1 removes one mark: {}", ok_or(removed));
        println!("  group 2 retries the refused directory: {}", ok_or(fan_mark(&g2, fan::MARK_ADD, fan::DESIGN_MASK, &d)));
    }
    0
}
