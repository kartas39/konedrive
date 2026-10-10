//! The unsent items a plan reads, a bounded number at a time, oldest first: what is left
//! here is not among them, and is not read to reach what can be sent.

use super::*;

fn unsent(store: &mut TreeStore, limit: usize) -> Vec<LocalItemId> {
    store.item_store().list_unsent(None, limit).unwrap().into_iter().map(|item| item.local_item_id).collect()
}

fn rename(store: &mut TreeStore, item: LocalItemId, parent: LocalItemId, name: &str, is_name_ignored: bool) {
    apply(store, LocalVersionChange::Moved { item, place: place(parent, name), is_name_ignored });
}

/// S3 of the second review, for an item's own name. A file whose name the upload rules
/// ignore, with no binding, keeps the stamp that says its local version differs, and is
/// not read among the unsent: a read of one item reaches the file that can be sent. When
/// it stops being left here — its name changes, or it gets a binding — it is read, where
/// its stamp puts it: before what came to be unsent after it.
#[test]
fn an_item_left_here_by_its_own_name_is_not_read_among_the_unsent() {
    let mut store = TreeStore::in_memory().unwrap();
    let left = make_named(&mut store, ItemKind::File, ROOT, "x.tmp", true);
    let sent = make_file(&mut store, ROOT, "sent");
    assert!(unsent_since(&mut store, left) < unsent_since(&mut store, sent));
    assert!(unsent_since(&mut store, left).is_some(), "the stamp says what differs");
    assert_eq!(unsent(&mut store, 1), [sent]);

    rename(&mut store, left, ROOT, "x.txt", false);
    assert_eq!(unsent(&mut store, 10), [left, sent], "its name is not ignored any more");
    rename(&mut store, left, ROOT, "x.tmp", true);
    assert_eq!(unsent(&mut store, 10), [sent]);
    store.item_store().bind_item(left, &reference("X")).unwrap();
    assert_eq!(unsent(&mut store, 10), [left, sent], "an item OneDrive has is never left here");
}

/// S3 of the second review, its own case. A directory whose name is ignored, with no
/// binding, holds many files: none of them is read among the unsent, nor read at all to
/// reach the file that can be sent. An item of OneDrive below such a directory is not left
/// here, and is read. When the directory stops being left here — its name changes, or it
/// gets a binding — what is below it is read, each where its stamp puts it; when it is
/// left here again, by its name or by giving its binding up, none of it is.
#[test]
fn what_is_below_a_directory_left_here_is_not_read_among_the_unsent() {
    let mut store = TreeStore::in_memory().unwrap();
    let cache = make_named(&mut store, ItemKind::Directory, ROOT, ".cache", true);
    let inner = make_directory(&mut store, cache, "inner");
    let files: Vec<LocalItemId> = (0..50).map(|n| make_file(&mut store, inner, &format!("f{n}"))).collect();
    let bound = make_file(&mut store, cache, "bound");
    store.item_store().bind_item(bound, &reference("B")).unwrap();
    let sent = make_file(&mut store, ROOT, "sent");
    let all: Vec<LocalItemId> = [cache, inner].into_iter().chain(files.clone()).chain([bound, sent]).collect();
    assert_eq!(unsent(&mut store, 2), [bound, sent]);
    assert_eq!(unsent(&mut store, 100), [bound, sent]);
    assert!(files.iter().all(|file| store.item_store().is_ignored(*file).unwrap()));

    rename(&mut store, cache, ROOT, "cache", false);
    assert_eq!(unsent(&mut store, 100), all, "in the order each came to be unsent");
    rename(&mut store, cache, ROOT, ".cache", true);
    assert_eq!(unsent(&mut store, 100), [bound, sent]);

    store.item_store().bind_item(cache, &reference("C")).unwrap();
    assert_eq!(unsent(&mut store, 100), all, "a directory OneDrive has leaves nothing here by its name");
    let taker = store.item_store().give_up_binding(cache, true).unwrap();
    assert_eq!(unsent(&mut store, 100), [bound, sent], "the directory OneDrive does not have any more is left here by its name");
    assert_eq!(item(&mut store, taker).local, None);
}

/// The same through each way an item comes to be below a directory that is left here, or
/// ceases to be: made there; moved in and out with what is below it; exchanged with an
/// item outside; and saved over a file in it. A name ignored further down leaves what is
/// below it here whatever the directory above becomes.
#[test]
fn an_item_is_left_here_by_the_directory_it_comes_to_be_in() {
    let mut store = TreeStore::in_memory().unwrap();
    let cache = make_named(&mut store, ItemKind::Directory, ROOT, ".cache", true);
    let dir = make_directory(&mut store, ROOT, "dir");
    let (deep, lock) = (make_file(&mut store, dir, "deep"), make_named(&mut store, ItemKind::Directory, dir, "lock.d", true));
    let below_lock = make_file(&mut store, lock, "below");
    assert_eq!(unsent(&mut store, 100), [dir, deep]);

    rename(&mut store, dir, cache, "dir", false);
    assert_eq!(unsent(&mut store, 100), Vec::new(), "moved into a directory that is left here, with what is below it");
    let made = make_file(&mut store, dir, "made");
    assert_eq!(unsent(&mut store, 100), Vec::new(), "made below it");
    rename(&mut store, dir, ROOT, "dir", false);
    assert_eq!(unsent(&mut store, 100), [dir, deep, made], "moved out; what a name further down leaves here stays left");
    assert!(store.item_store().is_ignored(below_lock).unwrap());

    let (inside, outside) = (make_file(&mut store, cache, "inside"), make_file(&mut store, ROOT, "outside"));
    assert_eq!(unsent(&mut store, 100), [dir, deep, made, outside]);
    store.item_store().exchange_local_places(inside, outside).unwrap();
    assert_eq!(unsent(&mut store, 100), [dir, deep, made, inside], "each has the other's place");
    apply(&mut store, LocalVersionChange::SavedOver { replacing: inside, replaced: outside });
    assert_eq!(unsent(&mut store, 100), [dir, deep, made], "saved over a file in the directory that is left here");
    assert_eq!(names(&mut store, cache), ["inside"]);
}
