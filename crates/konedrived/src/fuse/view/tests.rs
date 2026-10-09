use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::{Base, Detection, OutboxKind, OutboxOp, OutboxState};
use konedrive_tree::{Change, Kind, Placement, Row, TreeStore};

use super::{Dir, Entry, View};
use crate::folder::disk::HOLDING;
use crate::fuse::under::Under;

fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row { id: id.into(), parent_id: parent.map(str::to_owned), name: name.into(), kind, size: 1, mtime: 0, etag: Some("e".into()), ctag: Some("c".into()), quickxor: None, mime: None, placement: Placement::Placed }
}

/// A detection of item `id`, which the base has as `name` in the root.
pub(in crate::fuse) fn of(kind: OutboxKind, id: &str, name: &str, rel: &str, to: (Option<&str>, Option<&str>)) -> Detection {
    Detection {
        kind,
        item_id: Some(id.into()),
        object: None,
        rel: PathBuf::from(rel),
        base: Some(Base { etag: Some("e".into()), ctag: Some("c".into()), parent: Some("R".into()), name: Some(name.into()) }),
        target_parent: to.0.map(str::to_owned),
        target_name: to.1.map(str::to_owned),
        same_content: kind == OutboxKind::Move,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

/// The view of a drive with a folder `docs` that holds `c.txt`, and two files in its root,
/// with `detections` waiting in the outbox, over a folder on disk that holds nothing. The
/// store is filled before the view has it: a view reads what its own connection wrote only
/// once.
pub(in crate::fuse) fn view(detections: Vec<Detection>) -> (tempfile::TempDir, View) {
    let mut store = TreeStore::in_memory().unwrap();
    let changes = [
        Change::Root(row("R", None, "root", Kind::Folder)),
        Change::Upsert(row("D", Some("R"), "docs", Kind::Folder)),
        Change::Upsert(row("A", Some("R"), "a.txt", Kind::File)),
        Change::Upsert(row("B", Some("R"), "b.txt", Kind::File)),
        Change::Upsert(row("C", Some("D"), "c.txt", Kind::File)),
    ];
    store.commit_page(&changes, "next").unwrap();
    let ops: Vec<OutboxOp> = detections.into_iter().map(OutboxOp::Record).collect();
    store.outbox_apply(&ops, 0).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let under = Arc::new(Under::open(dir.path()).unwrap());
    (dir, View::new(store, under))
}

fn docs() -> Dir {
    Dir { id: Some("D".into()), rel: PathBuf::from("docs") }
}

fn names(view: &View, dir: &Dir) -> Vec<String> {
    let mut names: Vec<String> = view.entries(dir).iter().map(|entry| entry.name().to_owned()).collect();
    names.sort();
    names
}

fn item(entry: Option<Entry>) -> Option<(String, String)> {
    match entry? {
        Entry::Item(row) => Some((row.id, row.name)),
        Entry::Local(_) => None,
    }
}

fn pair(id: &str, name: &str) -> Option<(String, String)> {
    Some((id.to_owned(), name.to_owned()))
}

#[test]
fn an_item_is_shown_where_the_store_has_it() {
    let (_dir, view) = view(Vec::new());
    assert_eq!(item(view.entry(&view.root_dir(), "a.txt")), pair("A", "a.txt"));
    assert_eq!(item(view.entry(&docs(), "c.txt")), pair("C", "c.txt"));
    assert_eq!(names(&view, &view.root_dir()), ["a.txt", "b.txt", "docs"]);
    assert_eq!(names(&view, &docs()), ["c.txt"]);
    assert_eq!(view.place("C").as_deref(), Some(Path::new("docs/c.txt")));
    assert_eq!(item(view.resolve(Path::new("docs/c.txt"))), pair("C", "c.txt"));
    assert!(view.entry(&view.root_dir(), "none.txt").is_none() && view.resolve(Path::new("a.txt/below")).is_none());
}

#[test]
fn the_folder_itself_is_the_drives_root() {
    let (_dir, view) = view(Vec::new());
    assert_eq!(view.root_id().as_deref(), Some("R"));
    assert_eq!(item(view.resolve(Path::new(""))), pair("R", ""));
    assert_eq!(view.place("R").as_deref(), Some(Path::new("")));
    assert_eq!(view.row("A").and_then(|a| view.parent(&a)).map(|above| above.id).as_deref(), Some("R"));
    assert!(view.parent(&view.root()).is_none());
}

/// Until its move is sent the store has the item where it was; the folder shows it where
/// the user put it, and nowhere else.
#[test]
fn a_moved_item_is_shown_only_at_its_new_place() {
    let (_dir, view) = view(vec![of(OutboxKind::Move, "A", "a.txt", "docs/new.txt", (Some("D"), Some("new.txt")))]);
    assert!(view.entry(&view.root_dir(), "a.txt").is_none());
    assert_eq!(item(view.entry(&docs(), "new.txt")), pair("A", "new.txt"));
    assert_eq!(names(&view, &view.root_dir()), ["b.txt", "docs"]);
    assert_eq!(names(&view, &docs()), ["c.txt", "new.txt"]);
    assert_eq!(view.place("A").as_deref(), Some(Path::new("docs/new.txt")));
    assert!(view.resolve(Path::new("a.txt")).is_none());
}

/// What the user moved to a name is what the name is, though OneDrive has another item there.
#[test]
fn an_item_moved_over_another_items_name_is_what_the_name_shows() {
    let (_dir, view) = view(vec![of(OutboxKind::Move, "A", "a.txt", "b.txt", (Some("R"), Some("b.txt")))]);
    assert_eq!(item(view.entry(&view.root_dir(), "b.txt")), pair("A", "b.txt"));
    assert_eq!(names(&view, &view.root_dir()), ["b.txt", "docs"]);
}

#[test]
fn a_removed_item_is_shown_nowhere() {
    let (_dir, view) = view(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    assert!(view.entry(&view.root_dir(), "b.txt").is_none());
    assert_eq!(names(&view, &view.root_dir()), ["a.txt", "docs"]);
    assert!(view.place("B").is_none() && view.resolve(Path::new("b.txt")).is_none());
}

/// A file the folder under the mount holds at an item's name — its downloaded copy, or
/// anything else — is not a second name.
#[test]
fn a_name_an_item_and_an_object_both_have_is_the_items() {
    let (dir, view) = view(Vec::new());
    fs::write(dir.path().join("a.txt"), b"mine").unwrap();
    assert_eq!(item(view.entry(&view.root_dir(), "a.txt")), pair("A", "a.txt"));
    assert_eq!(names(&view, &view.root_dir()), ["a.txt", "b.txt", "docs"]);
}

#[test]
fn an_object_without_an_item_is_listed_where_it_lies() {
    let (dir, view) = view(Vec::new());
    fs::create_dir(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/own.txt"), b"mine").unwrap();
    fs::create_dir(dir.path().join("new")).unwrap();
    assert!(matches!(view.entry(&docs(), "own.txt"), Some(Entry::Local(local)) if local.name == "own.txt" && local.object.meta.is_file()));
    assert_eq!(names(&view, &docs()), ["c.txt", "own.txt"]);
    assert_eq!(names(&view, &view.root_dir()), ["a.txt", "b.txt", "docs", "new"]);
    assert!(matches!(view.resolve(Path::new("new")), Some(Entry::Local(local)) if local.object.meta.is_dir()));
}

/// The daemon's own names, a link, and a file that carries the id of an item the store has
/// are none of them the user's own.
#[test]
fn what_is_the_daemons_or_an_items_is_not_an_object_without_an_item() {
    let (dir, view) = view(Vec::new());
    fs::create_dir(dir.path().join(HOLDING)).unwrap();
    std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
    fs::write(dir.path().join("copy.txt"), b"a").unwrap();
    xattr::set(dir.path().join("copy.txt"), XATTR_ITEM_ID, b"A").unwrap();
    fs::write(dir.path().join("sent.txt"), b"a").unwrap();
    xattr::set(dir.path().join("sent.txt"), XATTR_ITEM_ID, b"unknown").unwrap();
    assert!(view.local(Path::new(HOLDING)).is_none() && view.local(Path::new("link")).is_none() && view.local(Path::new("copy.txt")).is_none());
    // An id the store does not have yet: its upload is being committed.
    assert!(view.local(Path::new("sent.txt")).is_some());
    assert_eq!(names(&view, &view.root_dir()), ["a.txt", "b.txt", "docs", "sent.txt"]);
}

#[test]
fn an_item_below_a_moved_folder_is_placed_through_the_folders_new_place() {
    let (_dir, view) = view(vec![of(OutboxKind::Move, "D", "docs", "papers", (Some("R"), Some("papers")))]);
    assert_eq!(view.place("C").as_deref(), Some(Path::new("papers/c.txt")));
    assert_eq!(item(view.resolve(Path::new("papers/c.txt"))), pair("C", "c.txt"));
    assert!(view.resolve(Path::new("docs/c.txt")).is_none());
}

/// A folder moved into a directory OneDrive has no item for yet is found by that directory's
/// place.
#[test]
fn an_item_below_a_folder_moved_into_a_new_directory_is_placed_below_that_directory() {
    let (dir, view) = view(vec![of(OutboxKind::Move, "D", "docs", "new/docs", (None, Some("docs")))]);
    fs::create_dir(dir.path().join("new")).unwrap();
    assert_eq!(view.place("C").as_deref(), Some(Path::new("new/docs/c.txt")));
    assert_eq!(item(view.resolve(Path::new("new/docs/c.txt"))), pair("C", "c.txt"));
}

#[test]
fn the_files_below_an_item_come_with_their_places() {
    let (_dir, view) = view(vec![of(OutboxKind::Delete, "B", "b.txt", "b.txt", (None, None))]);
    let below = |id: &str| {
        let mut files: Vec<(String, PathBuf)> = view.files_below(&view.row(id).unwrap()).into_iter().map(|(file, at)| (file.id, at)).collect();
        files.sort();
        files
    };
    assert_eq!(below("D"), [("C".to_owned(), PathBuf::from("docs/c.txt"))]);
    assert_eq!(below("A"), [("A".to_owned(), PathBuf::from("a.txt"))]);
    // The removed file is not in the folder.
    assert_eq!(below("R"), [("A".to_owned(), PathBuf::from("a.txt")), ("C".to_owned(), PathBuf::from("docs/c.txt"))]);
}

#[test]
fn the_directories_down_to_a_place_carry_the_items_the_view_has_there() {
    let (_dir, view) = view(vec![of(OutboxKind::Move, "D", "docs", "papers", (Some("R"), Some("papers")))]);
    view.backing(Path::new("papers/own")).unwrap();
    assert_eq!(view.under().carried(Path::new("papers")).as_deref(), Some("D"));
    assert_eq!(view.under().carried(Path::new("papers/own")), None);
}
