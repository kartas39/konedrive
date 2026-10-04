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

/// A helper that lets every question time out, and counts them.
#[derive(Default)]
struct Silent(AtomicUsize);

impl Liveness for Silent {
    fn whereabouts(&self, _handle: &FileHandle) -> std::io::Result<Whereabouts> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the helper did not answer"))
    }
}

/// A helper that does not answer costs an examination one timeout, not one for each missing
/// item: the tree lock is held all the while. After the first ask that times out nothing more
/// is asked in that run; nothing is deleted, every item waits, and the next run asks again.
#[test]
fn a_helper_that_times_out_is_asked_once_in_a_run() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b"), file("C", "R", "c.txt", b"c")]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::remove_file(fx.path(name)).unwrap();
    }
    let silent = Silent::default();
    let batch = names(&[("", "a.txt"), ("", "b.txt"), ("", "c.txt")]);
    let mut out = fx.examine_with(&batch, &silent);
    assert_eq!(silent.0.load(Ordering::SeqCst), 1, "one timeout for the run");
    out.undecided.sort();
    assert_eq!(out.undecided, ["A", "B", "C"], "what was not asked is not decided");
    assert!(fx.rows().is_empty(), "and nothing is deleted");

    fx.examine_with(&out.recheck, &silent);
    assert_eq!(silent.0.load(Ordering::SeqCst), 2, "the next run asks again, once");
    assert!(fx.rows().is_empty());

    // The helper is back: the same items are decided.
    fx.examine(&out.recheck);
    assert_eq!(fx.summary().iter().map(|(kind, ..)| *kind).collect::<Vec<_>>(), [Delete, Delete, Delete]);
}

/// The folder's filesystem changed (a new disk, a snapshot rolled back): a `move-out` row
/// takes the handle of what stands where its object was last proved, and only of what the
/// path itself names. A path that now leads through a symbolic link names nothing, whatever
/// the link points at: the row goes, the item stays in OneDrive, and nothing is deleted.
#[test]
fn on_a_changed_filesystem_a_place_reached_through_a_link_is_not_trusted() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b")]);
    let (a, b) = (fx.handle("a.txt"), fx.handle("b.txt"));
    let (a_now, b_now) = (fx.outside.join("a.txt"), fx.outside.join("sub/b.txt"));
    std::fs::create_dir(fx.outside.join("sub")).unwrap();
    for (name, handle, now) in [("a.txt", a, &a_now), ("b.txt", b, &b_now)] {
        std::fs::rename(fx.path(name), now).unwrap();
        fx.liveness.alive(handle, now);
    }
    fx.examine(&names(&[("", "a.txt"), ("", "b.txt")]));
    assert_eq!(fx.summary(), vec![(MoveOut, "a.txt".into(), Some("A".into())), (MoveOut, "b.txt".into(), Some("B".into()))]);

    // `sub` is now a link to the directory the object is in.
    std::fs::rename(fx.outside.join("sub"), fx.outside.join("real")).unwrap();
    std::os::unix::fs::symlink(fx.outside.join("real"), fx.outside.join("sub")).unwrap();
    assert_eq!(id_of(&b_now).as_deref(), Some("B"), "the object can be read through the link");
    fx.store.call_blocking(|s| s.set_handles_filesystem("root:0102")).unwrap();

    let out = fx.examine(&names(&[("", "a.txt")]));
    assert!(out.renewed, "the handles are taken again, by a scan of the whole folder");
    assert_eq!(fx.summary(), vec![(MoveOut, "a.txt".into(), Some("A".into()))], "the object at its own path keeps its row");
    assert_eq!(fx.row_at("a.txt").inode.and_then(|i| i.handle), Some(handle_at(&a_now)));
    let root = File::open(&fx.root.path).unwrap();
    let recorded = fx.store.call_blocking(|s| s.handles_filesystem()).unwrap();
    assert_eq!(recorded, Some(crate::local::handles::namespace(&root).unwrap()), "and the filesystem the folder is on now is recorded");
}

fn handle_at(path: &Path) -> FileHandle {
    FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
}
