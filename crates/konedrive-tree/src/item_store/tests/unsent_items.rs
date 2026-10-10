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
