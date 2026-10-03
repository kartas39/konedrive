//! `LO3`: an entry the examination cannot open, strip or read is passed over.
//! Its trouble never stops the batch, it does not count as missing, and it
//! is not uploaded as new.

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

/// Runs once a Full local scan has listed a directory: what it does to a file
/// happens after the file's entry was read, and before anything opens it.
struct AfterListing<F: Fn()>(F);

impl<F: Fn()> ScanProgress for AfterListing<F> {
    fn started(&self) {}

    fn seen(&self, _directories: u64, _files: u64) {
        (self.0)()
    }
}

/// A Full local scan during which `rels` lose every permission right after
/// they were listed. They have their modes back when this returns.
fn scan_locking(fx: &Fx, rels: &[&str]) -> Result<Examined, ExamineError> {
    let disk = fx.disk();
    let lock = AfterListing(|| rels.iter().for_each(|rel| set_mode(&fx.path(rel), 0o000)));
    let examiner = Examiner { disk: &disk, store: &fx.store, liveness: &fx.liveness, ignore: &fx.ignore, locks: &fx.locks, now: 1000 };
    let examined = examiner.examine_reporting(&Batch::full(), Some(&lock));
    rels.iter().for_each(|rel| set_mode(&fx.path(rel), 0o644));
    examined
}

/// LO3's case: a downloaded file whose time changed and which this daemon may
/// not read (`chmod 000`). Its attributes cannot be read by name either, so
/// it is passed over and reported before anything opens it: the rest of the
/// batch is examined, and the item does not count as missing.
#[test]
fn an_unreadable_downloaded_file_does_not_stop_the_examination() {
    if root() {
        return;
    }
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello")]);
    fx.hydrate("a.txt", b"hello");
    File::options().write(true).open(fx.path("a.txt")).unwrap().set_modified(SystemTime::now()).unwrap();
    fx.write("new.txt", b"n");
    set_mode(&fx.path("a.txt"), 0o000);
    let named = fx.try_examine(&fx.disk(), &names(&[("", "a.txt"), ("", "new.txt")]), &fx.liveness);
    let full = fx.try_examine(&fx.disk(), &Batch::full(), &fx.liveness);
    set_mode(&fx.path("a.txt"), 0o644);
    let named = named.expect("the places named are examined");
    let full = full.expect("the Full local scan is examined");
    assert_eq!(named.unreadable, vec![PathBuf::from("a.txt")]);
    assert_eq!(full.unreadable, vec![PathBuf::from("a.txt")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None)], "the new file goes up, and a.txt is neither changed nor deleted");
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

    // Readable again: the recheck finds the edit, and gives the placeholder its size back.
    let again = fx.examine(&out.recheck);
    assert!(again.unreadable.is_empty());
    assert_eq!(again.restored, vec![PathBuf::from("p.bin")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None), (Update, "a.txt".into(), Some("A".into()))]);
}

/// The narrow form of LO3, at the strip and at the probe for a writer: a copy
/// that kept its attributes and a new file, neither of which can be opened.
/// The copy still carries an id that is not its own, so it is not uploaded as
/// new; the new file gets no row. Both go up once they can be opened.
#[test]
fn a_stranger_that_cannot_be_stripped_is_not_uploaded() {
    if root() {
        return;
    }
    let fx = Fx::new(&[file("A", "R", "a.txt", b"hello")]);
    fx.hydrate("a.txt", b"hello");
    copy_keeping_attributes(&fx.path("a.txt"), &fx.path("copy.txt"));
    fx.write("new.txt", b"n");
    let out = scan_locking(&fx, &["copy.txt", "new.txt"]).expect("a copy that cannot be stripped does not stop the examination");
    let mut unreadable = out.unreadable.clone();
    unreadable.sort();
    assert_eq!(unreadable, vec![PathBuf::from("copy.txt"), PathBuf::from("new.txt")]);
    assert!(out.stripped.is_empty());
    assert_eq!(id_of(&fx.path("copy.txt")), Some("A".into()));
    assert!(fx.rows().is_empty(), "{:?}", fx.summary());

    let again = fx.examine(&out.recheck);
    assert_eq!(again.stripped, vec![PathBuf::from("copy.txt")]);
    assert_eq!(fx.summary(), vec![(Create, "new.txt".into(), None), (Create, "copy.txt".into(), None)]);
}
