//! `LO3`: an entry the examination cannot open, strip or read is passed over.
//! Its trouble never stops the batch, it does not count as missing, and it
//! is not uploaded as new. It is listed as not uploaded, and so is a file
//! whose marks are damaged, until the cause is gone.

use std::os::unix::fs::PermissionsExt;

use super::*;

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn root() -> bool {
    let root = unsafe { libc::geteuid() } == 0;
    if root {
        eprintln!("skipping: running as root, which chmod 000 cannot refuse");
    }
    root
}

/// The "not uploaded" list: each place and its reason as stored.
fn listed(fx: &Fx) -> Vec<(String, String)> {
    fx.store.call_blocking(|s| s.local_skipped()).unwrap().into_iter().map(|s| (s.rel.display().to_string(), s.reason.to_string())).collect()
}

fn cannot_be_read(rels: &[&str]) -> Vec<(String, String)> {
    rels.iter().map(|rel| (rel.to_string(), "unreadable".to_owned())).collect()
}

/// Runs once a Full local scan has listed a directory: what it does to a file
/// happens after the file's entry was read, and before anything opens it.
struct AfterListing<F: Fn(u64)>(F);

impl<F: Fn(u64)> ScanProgress for AfterListing<F> {
    fn started(&self) {}

    fn seen(&self, _directories: u64, files: u64) {
        (self.0)(files)
    }
}

/// A Full local scan during which `change` runs after each directory listed,
/// with the number of files read so far.
pub(super) fn scan_changing(fx: &Fx, change: impl Fn(u64)) -> Result<Examined, ExamineError> {
    let disk = fx.disk();
    let examiner = Examiner { disk: &disk, store: &fx.store, liveness: &fx.liveness, ignore: &fx.ignore, locks: &fx.locks, now: 1000 };
    examiner.examine_reporting(&Batch::full(), Some(&AfterListing(change)))
}

/// A Full local scan during which `rels` lose every permission right after
/// they were listed. They have their modes back when this returns.
fn scan_locking(fx: &Fx, rels: &[&str]) -> Result<Examined, ExamineError> {
    let examined = scan_changing(fx, |_| rels.iter().for_each(|rel| set_mode(&fx.path(rel), 0o000)));
    rels.iter().for_each(|rel| set_mode(&fx.path(rel), 0o644));
    examined
}

/// LO3's case: a downloaded file whose time changed and which this daemon may
/// not read (`chmod 000`). Its attributes cannot be read by name either, so
/// it is passed over and reported before anything opens it: the rest of the
/// batch is examined, and the item does not count as missing. It is listed
/// as not uploaded until it can be read; a file nobody may read whose name is
/// on the ignore list would stay local anyway, and is not listed.
#[test]
fn an_unreadable_downloaded_file_does_not_stop_the_examination() {
    if root() {
        return;
    }
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello")]);
    fx.hydrate("a.txt", b"hello");
    File::options().write(true).open(fx.path("a.txt")).unwrap().set_modified(SystemTime::now()).unwrap();
    fx.write("new.txt", b"n");
    fx.write("backup.txt~", b"b");
    set_mode(&fx.path("a.txt"), 0o000);
    set_mode(&fx.path("backup.txt~"), 0o000);
    let named = fx.try_examine(&fx.disk(), &names(&[("", "a.txt"), ("", "new.txt")]), &fx.liveness);
    let listed_by_names = listed(&fx);
    let full = fx.try_examine(&fx.disk(), &Batch::full(), &fx.liveness);
    let listed_by_scan = listed(&fx);
    set_mode(&fx.path("a.txt"), 0o644);
    set_mode(&fx.path("backup.txt~"), 0o644);
    let named = named.expect("the places named are examined");
    let mut full = full.expect("the Full local scan is examined");
    full.unreadable.sort();
    assert_eq!(named.unreadable, vec![PathBuf::from("a.txt")]);
    assert_eq!(full.unreadable, vec![PathBuf::from("a.txt"), PathBuf::from("backup.txt~")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None)], "the new file goes up, and a.txt is neither changed nor deleted");
    assert_eq!((listed_by_names, listed_by_scan), (cannot_be_read(&["a.txt"]), cannot_be_read(&["a.txt"])));

    // Readable again: the look its change of mode asks for takes the line off.
    fx.examine(&names(&[("", "a.txt")]));
    assert!(listed(&fx).is_empty(), "{:?}", listed(&fx));
}

/// LO3's other place: a copy that kept its attributes and is read-only. The
/// owner cannot take an attribute off a `0444` file as it is; the strip lifts
/// the mode for that one call, so the copy is uploaded as new and stays
/// read-only.
#[test]
fn a_read_only_copy_that_kept_its_attributes_is_stripped_and_uploaded_as_new() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello")]);
    fx.hydrate("a.txt", b"hello");
    copy_keeping_attributes(&fx.path("a.txt"), &fx.path("copy.txt"));
    set_mode(&fx.path("copy.txt"), 0o444);
    let out = fx.try_examine(&fx.disk(), &names(&[("", "copy.txt")]), &fx.liveness).expect("the copy does not stop the examination");
    assert_eq!(out.stripped, vec![PathBuf::from("copy.txt")]);
    assert_eq!(id_of(&fx.path("copy.txt")), None);
    assert_eq!(std::fs::metadata(fx.path("copy.txt")).unwrap().permissions().mode() & 0o7777, 0o444);
    assert_eq!(fx.summary(), vec![(Create, "copy.txt".into(), None)]);
}

/// The narrow form of LO3, at the content check and at the restore of a cut
/// placeholder: files of ours that were listed and then cannot be opened. They
/// are passed over and looked at again; the rest of the batch is examined, and
/// nothing is deleted in OneDrive because of them.
#[test]
fn items_that_cannot_be_opened_are_passed_over_and_examined_again() {
    if root() {
        return;
    }
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello"), file("P", "R", "p.bin", &[7u8; 100])]);
    fx.hydrate("a.txt", b"hello");
    fx.write("a.txt", b"HELLO");
    nix::unistd::truncate(&fx.path("p.bin"), 10).unwrap();
    fx.write("new.txt", b"n");
    let out = scan_locking(&fx, &["a.txt", "p.bin"]).expect("two files that cannot be opened do not stop the examination");
    let mut unreadable = out.unreadable.clone();
    unreadable.sort();
    assert_eq!(unreadable, vec![PathBuf::from("a.txt"), PathBuf::from("p.bin")]);
    assert!(out.restored.is_empty());
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None)], "the new file goes up; the two are neither changed nor deleted");
    assert_eq!(listed(&fx), cannot_be_read(&["a.txt", "p.bin"]));

    // Readable again: the recheck finds the edit, gives the placeholder its size back, and
    // takes both off the list.
    let again = fx.examine(&out.passed);
    assert!(again.unreadable.is_empty());
    assert!(listed(&fx).is_empty(), "{:?}", listed(&fx));
    assert_eq!(again.restored, vec![PathBuf::from("p.bin")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None), (Update, "a.txt".into(), Some("A".into()))]);
}

/// The narrow form of LO3, at the strip and at the probe for a writer: a copy
/// that kept its attributes and a new file, neither of which can be opened.
/// The copy still carries an id that is not its own, so it is not uploaded as
/// new; the new file gets no row. Both go up once they can be opened.
#[test]
fn a_copy_and_a_new_file_that_cannot_be_opened_get_no_row() {
    if root() {
        return;
    }
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello")]);
    fx.hydrate("a.txt", b"hello");
    copy_keeping_attributes(&fx.path("a.txt"), &fx.path("copy.txt"));
    fx.write("new.txt", b"n");
    let out = scan_locking(&fx, &["copy.txt", "new.txt"]).expect("a copy that cannot be opened does not stop the examination");
    let mut unreadable = out.unreadable.clone();
    unreadable.sort();
    assert_eq!(unreadable, vec![PathBuf::from("copy.txt"), PathBuf::from("new.txt")]);
    assert!(out.stripped.is_empty());
    assert_eq!(id_of(&fx.path("copy.txt")), Some("A".into()));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
    assert_eq!(listed(&fx), cannot_be_read(&["copy.txt", "new.txt"]));

    let again = fx.examine(&out.passed);
    assert!(listed(&fx).is_empty(), "{:?}", listed(&fx));
    assert_eq!(again.stripped, vec![PathBuf::from("copy.txt")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None), (Create, "copy.txt".into(), None)]);
}

/// An error that is not about the entry (here: the name is no longer a file;
/// in real use, no descriptors or memory left) fails the batch, as it always
/// did: it is offered again, and `LastError` says so if it keeps failing.
#[test]
fn an_error_that_is_not_the_entrys_own_fails_the_batch() {
    let fx = Fx::new(&[]);
    fx.write("new.txt", b"n");
    let swap = || {
        if fx.path("new.txt").is_file() {
            std::fs::remove_file(fx.path("new.txt")).unwrap();
            std::fs::create_dir(fx.path("new.txt")).unwrap();
        }
    };
    let examined = scan_changing(&fx, |_| swap());
    assert!(matches!(examined, Err(ExamineError::Io(_))), "{examined:?}");
    assert!(fx.rows().is_empty());
}

/// A directory that carries another folder's id and goes while the run looks
/// at it: what was listed inside it gets no row.
#[test]
fn what_was_listed_in_a_copied_folder_that_went_gets_no_row() {
    let fx = Fx::new(&[folder("D", "R", "d")]);
    std::fs::create_dir(fx.path("Copy")).unwrap();
    xattr::set(fx.path("Copy"), XATTR_ITEM_ID, b"D").unwrap();
    fx.write("Copy/new.txt", b"n");
    // Once `Copy/new.txt` has been read, `Copy` goes.
    let out = scan_changing(&fx, |files| {
        if files > 0 {
            let _ = std::fs::remove_dir_all(fx.path("Copy"));
        }
    })
    .expect("a directory that went does not stop the examination");
    assert!(out.stripped.is_empty() && out.unreadable.is_empty());
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());
}

/// A directory that cannot be read is listed, and a line of the "not uploaded" list
/// inside it stays: what could not be looked at is not known to be gone. The directory's
/// line goes when it can be read again, the line inside when the thing is no longer there.
#[test]
fn a_skipped_line_inside_a_directory_that_cannot_be_read_stays_listed() {
    if root() {
        return;
    }
    let fx = Fx::new(&[folder("D", "R", "photos")]);
    std::os::unix::fs::symlink("/etc/hostname", fx.path("photos/link")).unwrap();
    fx.examine(&Batch::full());
    let link = ("photos/link".to_owned(), "symlink".to_owned());
    assert_eq!(listed(&fx), [link.clone()]);

    set_mode(&fx.path("photos"), 0o000);
    let out = fx.examine(&Batch::full());
    // A look at the directory's own name alone, as a change of its mode asks for.
    fx.examine(&names(&[("", "photos")]));
    let closed = listed(&fx);
    set_mode(&fx.path("photos"), 0o755);
    assert_eq!(out.unreadable, [PathBuf::from("photos")]);
    assert_eq!(closed, [("photos".to_owned(), "unreadable".to_owned()), link.clone()], "the link is still there, only not seen");

    fx.examine(&names(&[("", "photos")]));
    assert_eq!(listed(&fx), [link], "the directory can be read; nothing looked inside it yet");

    std::fs::remove_file(fx.path("photos/link")).unwrap();
    fx.examine(&Batch::full());
    assert!(listed(&fx).is_empty());
}

/// A file of an item whose state mark is gone, or says nothing konedrive writes, is left
/// alone and listed, and so is a copy carrying such marks. The line goes when the state can
/// be read again.
#[test]
fn a_file_whose_marks_are_damaged_is_listed_until_they_can_be_read() {
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello"), file("B", "R", "b.txt", b"world")]);
    fx.hydrate("a.txt", b"hello");
    fx.hydrate("b.txt", b"world");
    xattr::remove(fx.path("a.txt"), placeholder::XATTR_STATE).unwrap();
    xattr::set(fx.path("b.txt"), placeholder::XATTR_STATE, b"no such state").unwrap();
    copy_keeping_attributes(&fx.path("b.txt"), &fx.path("copy.txt"));
    let out = fx.examine(&Batch::full());
    let damaged = |rels: &[&str]| rels.iter().map(|rel| (rel.to_string(), "state-unreadable".to_owned())).collect::<Vec<_>>();
    assert_eq!(listed(&fx), damaged(&["a.txt", "b.txt", "copy.txt"]));
    assert!(fx.rows().is_empty() && out.stripped.is_empty() && out.unreadable.is_empty(), "{:?}", fx.summary());
    assert_eq!(id_of(&fx.path("copy.txt")), Some("B".into()), "the copy is left as it is");

    for rel in ["a.txt", "b.txt", "copy.txt"] {
        xattr::set(fx.path(rel), placeholder::XATTR_STATE, b"hydrated").unwrap();
    }
    fx.examine(&Batch::full());
    assert!(listed(&fx).is_empty(), "{:?}", listed(&fx));
    assert_eq!(fx.summary(), vec![(Create, "copy.txt".into(), None)], "the copy can be read now: uploaded as new");
}
