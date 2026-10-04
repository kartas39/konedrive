//! Asking where a missing object is (`local/liveness.rs`), and which filesystem the recorded
//! handles belong to (`local/handles.rs`): an answer that did not come, or that cannot be
//! trusted, never reads as "gone".

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::helper::HelperError;
use crate::local::liveness::answered;

/// The helper's answer as the examination reads it: `ESTALE` is gone, a descriptor says
/// where, `EPERM` — never gone — decides nothing, and neither does a helper that is not
/// there. One that did not answer in time is told apart from every other failure.
#[test]
fn the_helpers_answer_is_read_as_the_examination_needs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("x");
    std::fs::write(&path, b"").unwrap();
    let fd: OwnedFd = File::open(&path).unwrap().into();
    assert_eq!(answered(Ok(fd)).unwrap(), Whereabouts::At(path));
    assert_eq!(answered(Err(HelperError::Refused(libc::ESTALE))).unwrap(), Whereabouts::Gone);
    assert_eq!(answered(Err(HelperError::Refused(libc::EPERM))).unwrap_err().raw_os_error(), Some(libc::EPERM));
    assert_ne!(answered(Err(HelperError::NotRunning)).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(answered(Err(HelperError::Timeout)).unwrap_err().kind(), std::io::ErrorKind::TimedOut);
}

/// A helper that lets every question time out, and counts them; `gone` is the one object it
/// does answer for.
#[derive(Default)]
struct Silent {
    timeouts: AtomicUsize,
    gone: Option<FileHandle>,
}

impl Silent {
    fn timeouts(&self) -> usize {
        self.timeouts.load(Ordering::SeqCst)
    }
}

impl Liveness for Silent {
    fn whereabouts(&self, handle: &FileHandle) -> std::io::Result<Whereabouts> {
        if self.gone.as_ref() == Some(handle) {
            return Ok(Whereabouts::Gone);
        }
        self.timeouts.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the helper did not answer"))
    }
}

/// A helper that does not answer costs an examination one timeout, not one for each missing
/// item: the tree lock is held all the while. After the first ask that times out nothing more
/// is asked in that run; nothing is deleted, every item waits, and the next run asks again.
/// The same when the helper falls silent while a deleted folder's items are asked after.
#[test]
fn a_helper_that_times_out_is_asked_once_in_a_run() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b"), file("C", "R", "c.txt", b"c")]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::remove_file(fx.path(name)).unwrap();
    }
    let silent = Silent::default();
    let batch = names(&[("", "a.txt"), ("", "b.txt"), ("", "c.txt")]);
    let mut out = fx.examine_with(&batch, &silent);
    assert_eq!(silent.timeouts(), 1, "one timeout for the run");
    out.undecided.sort();
    assert_eq!(out.undecided, ["A", "B", "C"], "what was not asked is not decided");
    assert!(fx.rows().is_empty(), "and nothing is deleted");

    fx.examine_with(&out.recheck, &silent);
    assert_eq!(silent.timeouts(), 2, "the next run asks again, once");
    assert!(fx.rows().is_empty());

    // The helper is back: the same items are decided.
    fx.examine(&out.recheck);
    assert_eq!(fx.summary().iter().map(|(kind, ..)| *kind).collect::<Vec<_>>(), [Delete, Delete, Delete]);

    // A folder that is gone, and a helper silent from its first item on.
    let fx = Fx::new(&[folder("D", "R", "docs"), file("X", "D", "x.txt", b"x"), file("Y", "D", "y.txt", b"y"), file("Z", "D", "z.txt", b"z")]);
    let silent = Silent { gone: Some(fx.handle("docs")), ..Silent::default() };
    std::fs::remove_dir_all(fx.path("docs")).unwrap();
    let out = fx.examine_with(&names(&[("", "docs")]), &silent);
    assert_eq!(silent.timeouts(), 1, "one timeout for the folder's items");
    assert_eq!(out.undecided, ["D"], "the folder waits for what is inside it");
    assert!(fx.rows().is_empty(), "and nothing is deleted");
}

/// The folder's filesystem changed (a new disk, a snapshot rolled back): a `move-out` row
/// takes the handle of the object where it went, also when the recorded path leads through a
/// symbolic link by now (the home copied to a new disk, and its old path made a link). What
/// proves the place is the item id the object carries: the user's move out goes on, with the
/// place as it is called now. A row whose object is not there goes, the item stays in
/// OneDrive, and Activity says so.
#[test]
fn on_a_changed_filesystem_a_move_out_goes_on_where_its_object_is_now() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b"), file("C", "R", "c.txt", b"c")]);
    let (a_now, b_now, c_now) = (fx.outside.join("a.txt"), fx.outside.join("sub/b.txt"), fx.outside.join("c.txt"));
    std::fs::create_dir(fx.outside.join("sub")).unwrap();
    for (name, now) in [("a.txt", &a_now), ("b.txt", &b_now), ("c.txt", &c_now)] {
        let handle = fx.handle(name);
        std::fs::rename(fx.path(name), now).unwrap();
        fx.liveness.alive(handle, now);
    }
    fx.examine(&names(&[("", "a.txt"), ("", "b.txt"), ("", "c.txt")]));
    assert_eq!(fx.summary().iter().map(|(kind, rel, _)| (*kind, rel.as_str())).collect::<Vec<_>>(), [(MoveOut, "a.txt"), (MoveOut, "b.txt"), (MoveOut, "c.txt")]);

    // `sub` is now a link to the directory B is in; another file stands where C went.
    std::fs::rename(fx.outside.join("sub"), fx.outside.join("real")).unwrap();
    std::os::unix::fs::symlink(fx.outside.join("real"), fx.outside.join("sub")).unwrap();
    std::fs::remove_file(&c_now).unwrap();
    std::fs::write(&c_now, b"another file").unwrap();
    fx.store.call_blocking(|s| s.set_handles_filesystem("root:0102")).unwrap();

    let out = fx.examine(&names(&[("", "a.txt")]));
    assert!(out.renewed, "the handles are taken again, by a scan of the whole folder");
    assert_eq!(fx.summary(), vec![(MoveOut, "a.txt".into(), Some("A".into())), (MoveOut, "b.txt".into(), Some("B".into()))]);
    assert_eq!(fx.row_at("a.txt").inode.and_then(|i| i.handle), Some(handle_at(&a_now)));
    let b_row = fx.row_at("b.txt");
    let b_real = fx.outside.join("real/b.txt");
    assert_eq!(b_row.inode.and_then(|i| i.handle), Some(handle_at(&b_real)), "found through the link");
    assert_eq!(b_row.target_name.as_deref(), b_real.to_str(), "and its place is the path with no link in it");
    let said = fx.store.call_blocking(|s| s.recent_activity(50)).unwrap();
    let said: Vec<_> = said.iter().filter(|event| event.kind == konedrive_tree::ActivityKind::Restored).map(|event| event.path.as_str()).collect();
    assert_eq!(said, [fx.path("c.txt").to_str().unwrap()], "what was given up is said");
    let root = File::open(&fx.root.path).unwrap();
    let recorded = fx.store.call_blocking(|s| s.handles_filesystem()).unwrap();
    assert_eq!(recorded, Some(crate::local::handles::namespace(&root).unwrap()), "and the filesystem the folder is on now is recorded");
}

fn handle_at(path: &Path) -> FileHandle {
    FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
}
