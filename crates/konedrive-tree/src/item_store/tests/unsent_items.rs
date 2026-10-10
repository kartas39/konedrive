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

/// Whether each row's item is left here, derived by the walk up the rows that the stored
/// fact stands for: the item has no binding, and its own name is ignored or that of a
/// directory above it in the folder that has no binding.
fn derive_left_here(rows: &[Item]) -> Vec<(LocalItemId, bool)> {
    let row_of = |id: LocalItemId| rows.iter().find(|row| row.local_item_id == id);
    let parent_of = |row: &Item| row.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id);
    let derive = |row: &Item| {
        if row.onedrive_item.is_some() {
            return false;
        }
        let mut is_left_here = row.is_name_ignored;
        let mut above = parent_of(row).and_then(row_of);
        while let Some(directory) = above {
            is_left_here |= directory.is_name_ignored && directory.onedrive_item.is_none();
            above = parent_of(directory).and_then(row_of);
        }
        is_left_here
    };
    rows.iter().map(|row| (row.local_item_id, derive(row))).collect()
}

/// The stored fact against its derivation, over every row, after each of a long sequence
/// of operations drawn from a seed: items made, moved, renamed to and from ignored names,
/// bound, giving their binding up, merged, exchanged, saved over and removed. And what
/// the reader of the unsent gives is what the derivation leaves, by stamp. A writer that
/// forgot the fact would drift here.
#[test]
fn whether_an_item_is_left_here_is_what_the_rows_above_it_say_after_any_sequence_of_operations() {
    for seed in [1_u64, 0x9E37_79B9_7F4A_7C15, 20_261_010] {
        let mut state = seed;
        let mut next = move |below: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % below as u64) as usize
        };
        let mut store = TreeStore::in_memory().unwrap();
        for step in 0..400 {
            let rows = store.item_store().list_items().unwrap();
            let shown: Vec<&Item> = rows.iter().filter(|row| row.local.is_some() && row.local_item_id != ROOT).collect();
            let directories: Vec<LocalItemId> = rows.iter().filter(|row| row.local.is_some() && row.kind == ItemKind::Directory).map(|row| row.local_item_id).collect();
            let directory = directories[next(directories.len())];
            let is_name_ignored = next(4) == 0;
            let name = format!("n{step}");
            let pick = |at: usize| shown.get(at % shown.len().max(1)).map(|row| (row.local_item_id, row.kind, row.local.clone().unwrap().place.unwrap()));
            let (first, second) = (pick(next(1000)), pick(next(1000)));
            let mut items = store.item_store();
            let done = match (next(10), first, second) {
                (0..=2, _, _) | (_, None, _) | (_, _, None) => {
                    let kind = if next(2) == 0 { ItemKind::Directory } else { ItemKind::File };
                    items.apply(&LocalVersionChange::Made { kind, place: place(directory, &name), content: (kind == ItemKind::File).then(made_here), is_name_ignored, replaced: None }).map(|_| ())
                }
                (3, Some((item, _, _)), _) => items.apply(&LocalVersionChange::Moved { item, place: place(directory, &name), is_name_ignored }).map(|_| ()),
                (4, Some((item, _, at)), _) => items.apply(&LocalVersionChange::Moved { item, place: at, is_name_ignored }).map(|_| ()),
                (5, Some((item, _, _)), _) => items.bind_item(item, &reference(&format!("B{step}"))),
                (6, Some((item, _, _)), _) => items.give_up_binding(item, is_name_ignored).map(|_| ()),
                (7, Some((item, kind, _)), _) => items.make_onedrive_item(&reference(&format!("M{step}")), kind).and_then(|listed| items.merge_items(item, listed)),
                (8, Some((first, _, _)), Some((second, _, _))) => items.exchange_local_places(first, second),
                (_, Some((first, _, _)), Some((second, _, _))) if next(2) == 0 => items.apply(&LocalVersionChange::SavedOver { replacing: first, replaced: second }).map(|_| ()),
                (_, Some((item, _, _)), _) => items.apply(&LocalVersionChange::Removed { item }).map(|_| ()),
            };
            assert!(matches!(done, Ok(()) | Err(TreeError::Refused(_))), "seed {seed}, step {step}: {done:?}");

            let rows = store.item_store().list_items().unwrap();
            let derived = derive_left_here(&rows);
            let stored: Vec<(LocalItemId, bool)> = rows.iter().map(|row| (row.local_item_id, store.item_store().is_ignored(row.local_item_id).unwrap())).collect();
            assert_eq!(stored, derived, "seed {seed}, step {step}");
            let mut sent: Vec<(u64, LocalItemId)> = rows.iter().zip(&derived).filter(|(_, (_, is_left_here))| !is_left_here).filter_map(|(row, _)| Some((row.unsent_since?, row.local_item_id))).collect();
            sent.sort();
            assert_eq!(unsent(&mut store, 10_000), sent.into_iter().map(|(_, id)| id).collect::<Vec<_>>(), "seed {seed}, step {step}");
        }
        let rows = store.item_store().list_items().unwrap();
        assert!(derive_left_here(&rows).iter().filter(|(_, is_left_here)| *is_left_here).count() > 5 && rows.iter().filter(|row| row.onedrive_item.is_some()).count() > 5, "seed {seed}: the sequence reached both");
    }
}

/// The brief's §6, 5: the guard against a mass removal asks of every unsent removal, also
/// of those a plan was not given.
#[test]
fn the_unsent_removals_are_counted_a_removed_directory_with_all_onedrive_has_in_it() {
    use crate::item_store::tests::listed_drive::{listed_directory, listed_file, reference, store_with_listed_root, write_listing_of_changes};
    let mut store = store_with_listed_root();
    write_listing_of_changes(
        &mut store,
        &[
            listed_directory("D", "R", "d", "e"),
            listed_file("A", "D", "a", (1, "h"), "e"),
            listed_directory("E", "D", "e", "e"),
            listed_file("B", "E", "b", (1, "h"), "e"),
            listed_file("C", "E", "c", (1, "h"), "e"),
            listed_file("F", "R", "f", (1, "h"), "e"),
            listed_file("G", "R", "g", (1, "h"), "e"),
        ],
    );
    let [directory, in_directory, inner, in_inner, moved_out, in_root, stays] = ["D", "A", "E", "B", "C", "F", "G"].map(|item_id| {
        let row = store.item_store().find_bound_item(&reference(item_id)).unwrap().unwrap();
        store.item_store().commit_show_item(&crate::ItemVersions::of(&row), false).unwrap();
        row.local_item_id
    });
    let none = std::collections::BTreeSet::new();
    assert_eq!(store.item_store().count_shown_items().unwrap(), 7);
    assert_eq!(store.item_store().count_unsent_removals(&none).unwrap(), 0);

    // A file of the root; then the directory, out of which one file was moved here first.
    let removed = |store: &mut TreeStore, item: LocalItemId| store.item_store().apply(&LocalVersionChange::Removed { item }).unwrap();
    removed(&mut store, in_root);
    assert_eq!(store.item_store().count_unsent_removals(&none).unwrap(), 1);
    store.item_store().apply(&LocalVersionChange::Moved { item: moved_out, place: place(ROOT, "c"), is_name_ignored: false }).unwrap();
    for item in [in_inner, inner, in_directory, directory] {
        removed(&mut store, item);
    }
    assert_eq!(store.item_store().count_shown_items().unwrap(), 2);
    assert_eq!(store.item_store().count_unsent_removals(&none).unwrap(), 5, "the four of the directory and the file; not what was moved out of it");

    // What OneDrive made in the directory since goes with it, and is counted.
    write_listing_of_changes(&mut store, &[listed_file("N", "E", "new", (1, "h"), "e")]);
    assert_eq!(store.item_store().count_unsent_removals(&none).unwrap(), 6);

    // A removal the user confirmed is left out, a directory's with all OneDrive has in it.
    assert_eq!(store.item_store().count_unsent_removals(&[in_root].into()).unwrap(), 5);
    assert_eq!(store.item_store().count_unsent_removals(&[directory].into()).unwrap(), 1);
    assert_eq!(store.item_store().count_unsent_removals(&[inner, stays].into()).unwrap(), 3);
}

/// Found by the convergence test: an item both sides agree on, whose placement a listing
/// changed, is in neither index a plan reads from, and stayed in the folder for good.
#[test]
fn the_shown_items_whose_placement_says_the_folder_cannot_show_them_are_read() {
    use crate::item_store::tests::listed_drive::{listed_directory, listed_file, not_placed, reference, store_with_listed_root, write_listing_of_changes};
    let mut store = store_with_listed_root();
    let listed = [listed_directory("D", "R", "d", "e"), listed_file("A", "D", "a", (1, "h"), "e"), listed_file("B", "R", "b", (1, "h"), "e"), listed_file("C", "R", "c", (1, "h"), "e")];
    write_listing_of_changes(&mut store, &listed);
    let [directory, _, file, _] = ["D", "A", "B", "C"].map(|item_id| {
        let row = store.item_store().find_bound_item(&reference(item_id)).unwrap().unwrap();
        store.item_store().commit_show_item(&crate::ItemVersions::of(&row), false).unwrap();
        row.local_item_id
    });
    let read = |store: &mut TreeStore, after: Option<LocalItemId>, limit: usize| -> Vec<LocalItemId> { store.item_store().list_shown_items_that_cannot_be_shown(after, limit).unwrap().iter().map(|item| item.local_item_id).collect() };
    assert_eq!(read(&mut store, None, 10), []);
    // A new item of OneDrive the folder cannot show was never shown: it is not among them.
    write_listing_of_changes(&mut store, &[not_placed(listed[0].clone()), not_placed(listed[2].clone()), not_placed(listed_file("N", "R", "n", (1, "h"), "e"))]);
    assert!(store.item_store().list_unsent(None, 10).unwrap().is_empty() && store.item_store().list_changed_in_onedrive(None, 10).unwrap().iter().all(|item| item.local.is_none()));
    assert_eq!(read(&mut store, None, 10), [directory, file]);
    assert_eq!(read(&mut store, None, 1), [directory]);
    assert_eq!(read(&mut store, Some(directory), 10), [file]);
}
