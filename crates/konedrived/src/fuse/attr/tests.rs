use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::FileType;
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::Kind;

use super::{attr, OnDisk, Shown};
use crate::fuse::directory_contents::tests::{of, view};
use crate::fuse::directory_contents::{Entry, Local, DirectoryContents};
use crate::fuse::writers::Writers;

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// A file of 10 bytes in 8 blocks, last changed at 500.
fn copy() -> OnDisk {
    OnDisk { size: 10, blocks: 8, changed: at(500) }
}

#[test]
fn a_folder_is_a_directory_with_the_stores_time_and_no_size() {
    let shown = attr(7, true, &Shown::Folder { listed: at(100) });
    assert_eq!((shown.ino.0, shown.kind, shown.size, shown.blocks, shown.nlink), (7, FileType::Directory, 0, 0, 2));
    assert_eq!((shown.atime, shown.mtime, shown.ctime), (at(100), at(100), at(100)));
}

#[test]
fn a_file_that_is_not_downloaded_has_the_stores_size_and_time_and_no_blocks() {
    let shown = attr(7, true, &Shown::NotDownloaded { size: 42, listed: at(100) });
    assert_eq!((shown.kind, shown.size, shown.blocks, shown.nlink, shown.mtime), (FileType::RegularFile, 42, 0, 1, at(100)));
}

/// A download does not change when the file was last changed: that is OneDrive's to say.
#[test]
fn a_clean_copy_has_its_own_size_and_blocks_and_the_stores_time() {
    let shown = attr(7, true, &Shown::Copy { copy: copy(), listed: at(100), own: false });
    assert_eq!((shown.kind, shown.size, shown.blocks, shown.mtime), (FileType::RegularFile, 10, 8, at(100)));
}

#[test]
fn a_copy_with_the_users_own_content_has_its_own_size_and_time() {
    let shown = attr(7, true, &Shown::Copy { copy: copy(), listed: at(100), own: true });
    assert_eq!((shown.size, shown.blocks, shown.atime, shown.mtime, shown.ctime), (10, 8, at(500), at(500), at(500)));
}

#[test]
fn an_object_without_an_item_has_what_the_filesystem_says_of_it() {
    let file = attr(7, true, &Shown::Local { object: copy(), dir: false });
    assert_eq!((file.kind, file.size, file.blocks, file.mtime), (FileType::RegularFile, 10, 8, at(500)));
    let dir = attr(7, true, &Shown::Local { object: copy(), dir: true });
    assert_eq!((dir.kind, dir.size, dir.blocks, dir.nlink, dir.mtime), (FileType::Directory, 0, 8, 2, at(500)));
}

/// A read-only folder shows no write bit anywhere.
#[test]
fn the_modes_say_whether_the_folder_takes_changes() {
    let (folder, file) = (Shown::Folder { listed: at(0) }, Shown::NotDownloaded { size: 1, listed: at(0) });
    assert_eq!((attr(2, true, &folder).perm, attr(2, true, &file).perm), (0o755, 0o644));
    assert_eq!((attr(2, false, &folder).perm, attr(2, false, &file).perm), (0o555, 0o444));
    let local = Shown::Local { object: copy(), dir: true };
    assert_eq!((attr(2, true, &local).perm, attr(2, false, &local).perm), (0o755, 0o555));
}

#[test]
fn an_item_that_is_gone_is_shown_by_its_kind_alone() {
    assert_eq!(Shown::gone(Kind::Folder), Shown::Folder { listed: UNIX_EPOCH });
    assert_eq!(Shown::gone(Kind::File), Shown::NotDownloaded { size: 0, listed: UNIX_EPOCH });
}

fn item(view: &DirectoryContents, id: &str) -> Entry {
    Entry::Item(view.row(id).unwrap())
}

/// The facts of the drive's `a.txt` (item `A`, one byte in the store) as node 7.
fn facts(view: &DirectoryContents, writers: &Writers) -> Shown {
    Shown::of(&item(view, "A"), 7, Path::new("a.txt"), view, writers)
}

/// The whole copy of item `A`, of five bytes, in the folder under the mount.
fn downloaded(dir: &Path, view: &DirectoryContents) {
    fs::write(dir.join("a.txt"), b"whole").unwrap();
    view.under().mark_copy(Path::new("a.txt"), "A").unwrap();
}

#[test]
fn the_facts_of_an_item_are_the_stores_until_the_folder_holds_its_copy() {
    let (dir, view) = view(Vec::new());
    let writers = Writers::default();
    assert_eq!(facts(&view, &writers), Shown::NotDownloaded { size: 1, listed: UNIX_EPOCH });
    assert_eq!(Shown::of(&item(&view, "D"), 3, Path::new("docs"), &view, &writers), Shown::Folder { listed: UNIX_EPOCH });
    // A file of the user's own at the item's place is not its copy.
    fs::write(dir.path().join("a.txt"), b"mine").unwrap();
    assert_eq!(facts(&view, &writers), Shown::NotDownloaded { size: 1, listed: UNIX_EPOCH });
    downloaded(dir.path(), &view);
    assert!(matches!(facts(&view, &writers), Shown::Copy { copy, own: false, .. } if copy.size == 5));
}

#[test]
fn a_copy_is_the_users_own_while_it_is_written_or_its_change_waits() {
    let (dir, view) = view(Vec::new());
    downloaded(dir.path(), &view);
    let writers = Writers::default();
    writers.opened(7, false);
    assert!(matches!(facts(&view, &writers), Shown::Copy { own: true, .. }));
    writers.closed(7);
    assert!(matches!(facts(&view, &writers), Shown::Copy { own: false, .. }));

    let (dir, view) = crate::fuse::directory_contents::tests::view(vec![of(OutboxKind::Update, "A", "a.txt", "a.txt", (Some("R"), Some("a.txt")))]);
    downloaded(dir.path(), &view);
    assert!(matches!(facts(&view, &Writers::default()), Shown::Copy { own: true, .. }));
}

#[test]
fn the_facts_of_an_object_without_an_item_are_its_own() {
    let (dir, view) = view(Vec::new());
    fs::write(dir.path().join("own.txt"), b"mine").unwrap();
    let object = view.under().object(Path::new("own.txt")).unwrap();
    let entry = Entry::Local(Local { name: "own.txt".into(), object });
    assert!(matches!(Shown::of(&entry, 9, Path::new("own.txt"), &view, &Writers::default()), Shown::Local { object, dir: false } if object.size == 4));
}
