use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use crate::item_store::tests::listed_drive::store_with_listed_root;
use crate::item_store::tests::shown_items::{assert_rules_hold, row, show_listed_directory, show_listed_file};
use crate::item_store::tests::{make_directory, make_file};
use crate::model::{LocalItemId, LocalVersionChange};
use crate::TreeStore;

const ROOT: LocalItemId = LocalItemId::ROOT;

/// A list that ignores the names ending in `.tmp` and the name `cache`.
fn ignores_tmp_and_cache(name: &OsStr) -> bool {
    name.as_bytes().ends_with(b".tmp") || name == "cache"
}

fn ignores_nothing(_: &OsStr) -> bool {
    false
}

fn unsent(store: &mut TreeStore) -> Vec<LocalItemId> {
    store.item_store().list_unsent(None, 100).unwrap().into_iter().map(|item| item.local_item_id).collect()
}

fn is_left_here(store: &mut TreeStore, item: LocalItemId) -> bool {
    store.item_store().is_ignored(item).unwrap()
}

#[test]
fn an_unbound_item_whose_name_the_new_list_ignores_is_left_here_with_what_is_below_it() {
    let mut store = TreeStore::in_memory().unwrap();
    let lock = make_file(&mut store, ROOT, "x.tmp");
    let cache = make_directory(&mut store, ROOT, "cache");
    let inner = make_directory(&mut store, cache, "inner");
    let below = make_file(&mut store, inner, "below");
    let sent = make_file(&mut store, ROOT, "sent");
    assert_eq!(unsent(&mut store), [lock, cache, inner, below, sent]);

    assert_eq!(store.item_store().rewrite_ignored_names(&ignores_tmp_and_cache).unwrap(), 2, "the rows whose own flag changed");
    assert_eq!(unsent(&mut store), [sent]);
    assert_eq!([lock, cache, inner, below, sent].map(|item| is_left_here(&mut store, item)), [true, true, true, true, false]);
    assert_eq!([lock, cache, inner, below].map(|item| row(&mut store, item).is_name_ignored), [true, true, false, false], "the flag is of the item's own name");
    assert_eq!(store.item_store().rewrite_ignored_names(&ignores_tmp_and_cache).unwrap(), 0, "gone over again with the same list");
    assert_rules_hold(&mut store);
}

#[test]
fn an_unbound_item_whose_name_the_new_list_no_longer_ignores_is_unsent_again() {
    let mut store = TreeStore::in_memory().unwrap();
    let lock = make_file(&mut store, ROOT, "x.tmp");
    let cache = make_directory(&mut store, ROOT, "cache");
    let below = make_file(&mut store, cache, "below");
    let sent = make_file(&mut store, ROOT, "sent");
    store.item_store().rewrite_ignored_names(&ignores_tmp_and_cache).unwrap();
    let late = make_file(&mut store, ROOT, "late");
    assert_eq!(unsent(&mut store), [sent, late]);

    assert_eq!(store.item_store().rewrite_ignored_names(&ignores_nothing).unwrap(), 2);
    assert_eq!(unsent(&mut store), [lock, cache, below, sent, late], "each where the stamp it kept puts it");
    assert!(![lock, cache, below].iter().any(|item| is_left_here(&mut store, *item)));
}

#[test]
fn an_item_onedrive_has_keeps_being_sent_whatever_the_list_says_of_its_name() {
    let mut store = store_with_listed_root();
    let cache = show_listed_directory(&mut store, "C", "R", "cache");
    let bound_file = show_listed_file(&mut store, "F", "C", "y.tmp", "h1");
    let made_below = make_file(&mut store, cache, "made");
    store.item_store().apply(&LocalVersionChange::Moved { item: bound_file, place: crate::item_store::tests::place(cache, "z.tmp"), is_name_ignored: false }).unwrap();

    assert_eq!(store.item_store().rewrite_ignored_names(&ignores_tmp_and_cache).unwrap(), 2);
    assert!(row(&mut store, cache).is_name_ignored && row(&mut store, bound_file).is_name_ignored, "the flag says what the rules say of the name");
    assert_eq!([cache, bound_file, made_below].map(|item| is_left_here(&mut store, item)), [false, false, false], "an item OneDrive has is never left here, nor what is in a directory OneDrive has");
    assert_eq!(unsent(&mut store), [made_below, bound_file]);
    assert_rules_hold(&mut store);
}

/// Whether `item` is left here, by a walk up the rows: its own name ignored, or that of a
/// directory above it, none of them bound.
fn is_left_here_by_walk(store: &mut TreeStore, item: LocalItemId) -> bool {
    let own = row(store, item);
    if own.onedrive_item.is_some() {
        return false;
    }
    let mut at = Some(own);
    while let Some(here) = at {
        if here.onedrive_item.is_none() && here.is_name_ignored {
            return true;
        }
        at = here.local.and_then(|local| local.place).map(|place| row(store, place.parent_local_item_id));
    }
    false
}

#[test]
fn the_flag_below_an_ignored_directory_equals_the_walk_up_after_the_go_over() {
    let mut store = store_with_listed_root();
    let bound_directory = show_listed_directory(&mut store, "D", "R", "cache");
    let mut all = vec![bound_directory, show_listed_file(&mut store, "F", "D", "f.tmp", "h1")];
    for (parent, name) in [(ROOT, "top"), (bound_directory, "in-bound")] {
        let directory = make_directory(&mut store, parent, name);
        let cache = make_directory(&mut store, directory, "cache");
        let deep = make_directory(&mut store, cache, "deep");
        all.extend([directory, cache, deep, make_file(&mut store, directory, "a.tmp"), make_file(&mut store, cache, "b"), make_file(&mut store, deep, "c.tmp"), make_file(&mut store, deep, "d")]);
    }
    let non_utf8 = store.item_store().apply(&crate::item_store::tests::made(crate::model::ItemPlace { parent_local_item_id: ROOT, name: OsStr::from_bytes(b"\xff.tmp").to_owned() }, crate::model::ItemKind::File)).unwrap();
    all.push(non_utf8);

    for list in [&ignores_tmp_and_cache as &dyn Fn(&OsStr) -> bool, &ignores_nothing, &ignores_tmp_and_cache] {
        store.item_store().rewrite_ignored_names(list).unwrap();
        for item in all.clone() {
            let name = row(&mut store, item).local.unwrap().place.unwrap().name;
            assert_eq!(row(&mut store, item).is_name_ignored, list(&name), "the flag of {name:?}");
            assert_eq!(is_left_here(&mut store, item), is_left_here_by_walk(&mut store, item), "{name:?}, item {item}");
        }
        assert_rules_hold(&mut store);
    }
    assert!(is_left_here(&mut store, non_utf8), "a name that is no UTF-8 is asked as it is");
}
