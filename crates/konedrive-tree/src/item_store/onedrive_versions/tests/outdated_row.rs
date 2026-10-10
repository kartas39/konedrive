//! A row a page leaves to be read again (`Outdated`), against everything else the same
//! page or its end can do: one test a cell. The rule every cell holds: a call takes
//! nothing through a row it leaves. Each cell says what the rows are after the call, after
//! the call that gives what was read again, and after "the daemon died and the page was
//! asked again".
//!
//! The rows every cell starts from, as a listing left them: the root `R` holds the
//! directory `G`, that holds `D`, that holds `X`; `X` holds the files `C` and `K` and the
//! directory `S`, which holds the file `F`. The folder shows `K`, and the two sides agree
//! on it. A step's rename of `X` to `x2` was confirmed after the fetch began: the row left.

use super::*;

/// The rows the cells start from, before anything is confirmed. `is_x_shown`: the folder
/// shows `X` too.
fn store_of_tree(is_x_shown: bool) -> TreeStore {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    if is_x_shown {
        let x = make_directory(&mut store, ROOT, "x-here");
        synced(&mut store, x, "X");
    }
    let tree = [directory("G", "R", "G"), directory("D", "G", "D"), directory("X", "D", "x"), file("C", "X", "c", "1"), file("K", "X", "kept", "1"), directory("S", "X", "s"), file("F", "S", "f", "1")];
    write_fetched_at(&mut store, &tree, 0, &delta_link("link-1"));
    store
}

/// A step's rename of `X` to `name` in `D` is confirmed, under an eTag no listing gives.
/// What the two sides agree on is not written here: the place agreed on names no row of
/// the tree, so that a row is seen to go when nothing of OneDrive's names it.
fn confirm_rename_of_x(store: &mut TreeStore, name: &str) -> u64 {
    let (x, d) = (listed(store, "X").local_item_id, listed(store, "D").local_item_id);
    store.item_store().confirm_onedrive_version(x, &present(d, name, None, &format!("e-X-ours-{name}"))).unwrap()
}

/// [`store_of_tree`] with the rename of `X` to `x2` confirmed at the count 1: a page
/// fetched at the count 0 that says anything else of `X` leaves it.
fn store_of_confirmed_x(is_x_shown: bool) -> TreeStore {
    let mut store = store_of_tree(is_x_shown);
    assert_eq!(confirm_rename_of_x(&mut store, "x2"), 1);
    assert_eq!(said(&mut store), CONFIRMED);
    store
}

/// What the rows say OneDrive has when the rename of `X` is confirmed ([`said`]).
const CONFIRMED: &str = "C=X/c D=G/D F=S/f G=R/G K=X/kept S=X/s X=D/x2";

/// What a page fetched before the confirmation says of `X`: another client moved it to
/// the root.
fn x_moved_to_root() -> Change {
    directory("X", "R", "x2")
}

/// What the rows say OneDrive has, in OneDrive's own words: each bound row but the
/// root's by its item id, with the item id of its directory and its name there, or `-`
/// when the row has no OneDrive version; by item id.
fn said(store: &mut TreeStore) -> String {
    let rows = every_row(store);
    let item_id_of = |id: LocalItemId| rows.iter().find(|row| row.local_item_id == id).and_then(|row| row.onedrive_item.as_ref()).map_or("?".to_owned(), |reference| reference.item_id.as_str().to_owned());
    let mut said: Vec<String> = rows
        .iter()
        .filter(|row| row.local_item_id != ROOT)
        .filter_map(|row| {
            let item_id = row.onedrive_item.as_ref()?.item_id.as_str();
            Some(match row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()) {
                Some(place) => format!("{item_id}={}/{}", item_id_of(place.parent_local_item_id), place.name.to_string_lossy()),
                None => format!("{item_id}=-"),
            })
        })
        .collect();
    said.sort();
    said.join(" ")
}

/// What a call left: the items to read again, what the rows say ([`said`]), and the
/// delta link the store has.
type Left = (Vec<String>, String, Option<String>);

/// Gives the page `changes`, fetched at `fetch_began_at_count` and followed by `link`.
fn give(store: &mut TreeStore, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> Left {
    let outdated_items = write_fetched_at(store, changes, fetch_began_at_count, link);
    (outdated_items, said(store), store.delta_link().unwrap())
}

fn left(outdated_items: &[&str], said: &str, delta_link: Option<&str>) -> Left {
    (outdated_items.iter().map(|id| (*id).to_owned()).collect(), said.to_owned(), delta_link.map(str::to_owned))
}

/// The cell with nothing beside the row: the page names `X` alone. The call leaves the
/// rows and the link as they were; what was read again is written with the link; and the
/// page asked again, after the confirmation, is written whole.
#[test]
fn a_row_left_by_a_page_that_says_nothing_else() {
    const AT_ROOT: &str = "C=X/c D=G/D F=S/f G=R/G K=X/kept S=X/s X=R/x2";
    let page = [x_moved_to_root()];
    let after_call = || {
        let mut store = store_of_confirmed_x(false);
        assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], CONFIRMED, Some("link-1")));
        store
    };
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "read again");
    assert_eq!(give(&mut after_call(), &page, 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "the page asked again");
}

/// M1 of the second review. The same page says the directory `X` was in is gone. What is
/// below `X` keeps its OneDrive version; the row of the removed directory stays, with no
/// version, while the row of `X` names it, as a parent named and never listed does; the
/// call that at last writes where `X` is deletes it; and the page asked again converges.
#[test]
fn a_row_left_and_the_directory_it_is_in_listed_gone() {
    const AT_ROOT: &str = "C=X/c F=S/f G=R/G K=X/kept S=X/s X=R/x2";
    let page = [x_moved_to_root(), gone("D")];
    let after_call = || {
        let mut store = store_of_confirmed_x(false);
        assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2", Some("link-1")));
        store
    };
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "read again");
    assert_eq!(give(&mut after_call(), &page, 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "the page asked again");
}

/// The same with the directory two above `X` listed gone, and the one between them named
/// by nothing: that one has no version and stays for `X`; what is below `X` is kept.
#[test]
fn a_row_left_and_a_directory_two_above_it_listed_gone() {
    const AT_ROOT: &str = "C=X/c F=S/f K=X/kept S=X/s X=R/x2";
    let page = [x_moved_to_root(), gone("G")];
    let after_call = || {
        let mut store = store_of_confirmed_x(false);
        assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f K=X/kept S=X/s X=D/x2", Some("link-1")));
        store
    };
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "read again");
    assert_eq!(give(&mut after_call(), &page, 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "the page asked again");
}

/// A page that does not name `X` says the directory it is in is gone: the removal's walk
/// finds `X`, leaves it and answers it, and goes no further. Read again, `X` is gone, and
/// that removal takes what is below it; or it is somewhere, and what is below it is
/// there. The page asked again after the confirmation takes all.
#[test]
fn a_row_left_by_the_removal_of_the_directory_it_is_in() {
    let page = [gone("D")];
    let after_call = || {
        let mut store = store_of_confirmed_x(false);
        assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2", Some("link-1")));
        store
    };
    assert_eq!(give(&mut after_call(), &[gone("X")], 1, &delta_link("link-2")), left(&[], "G=R/G K=- X=-", Some("link-2")), "read again: gone, and the row that was stamped stays until a listing ends that began after");
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], "C=X/c F=S/f G=R/G K=X/kept S=X/s X=R/x2", Some("link-2")), "read again: at the root");
    assert_eq!(give(&mut after_call(), &page, 1, &delta_link("link-2")), left(&[], "G=R/G K=- X=-", Some("link-2")), "the page asked again");
}

/// The rows after a page fetched before the confirmation said the directory two above `X`
/// is gone, and named nothing else: the walk passed the directory between, which nothing
/// was confirmed of, and stopped at `X`. The removed directory's row went, as no row names
/// it; the one between has no version and stays for `X`.
fn store_after_the_removal_of_a_directory_two_above_x() -> TreeStore {
    let mut store = store_of_confirmed_x(false);
    assert_eq!(give(&mut store, &[gone("G")], 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f K=X/kept S=X/s X=D/x2", Some("link-1")));
    store
}

/// The same from two above, and `X` read again: gone, and the removal takes what is below.
#[test]
fn a_row_left_by_the_removal_of_a_directory_two_above_it() {
    let mut store = store_after_the_removal_of_a_directory_two_above_x();
    assert_eq!(give(&mut store, &[gone("X")], 1, &delta_link("link-2")), left(&[], "K=- X=-", Some("link-2")), "read again: gone");
}

/// The cell that does not hold. The same from two above, and `X` is never read again: the
/// page asked again says the directory two above is gone, whose row went with the first
/// call, so nothing leads from it to `X`. The rows go on saying OneDrive has `X` and what
/// is below it, in a directory that has no version, and the listing ends. A removal whose
/// walk stops at a row keeps no way back to that row once the removed directory is more
/// than one above it.
#[test]
#[ignore = "does not hold: the removed directory's row went with the first call, and the page asked again finds no way to the row that was left"]
fn a_row_left_by_the_removal_of_a_directory_two_above_it_and_the_page_asked_again() {
    let mut store = store_after_the_removal_of_a_directory_two_above_x();
    assert_eq!(give(&mut store, &[gone("G")], 1, &delta_link("link-2")), left(&[], "K=- X=-", Some("link-2")), "the page asked again");
}

/// The page says `X` itself is gone: it is left, and what is below it with it. Read
/// again it is there; or the page asked again says it is gone, and takes what is below.
#[test]
fn a_row_left_by_its_own_removal() {
    let page = [gone("X")];
    let after_call = || {
        let mut store = store_of_confirmed_x(false);
        assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], CONFIRMED, Some("link-1")));
        store
    };
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], "C=X/c D=G/D F=S/f G=R/G K=X/kept S=X/s X=R/x2", Some("link-2")), "read again");
    assert_eq!(give(&mut after_call(), &page, 1, &delta_link("link-2")), left(&[], "D=G/D G=R/G K=- X=-", Some("link-2")), "the page asked again: the row that was stamped stays until a listing ends that began after");
}

/// A full listing's last page names `X` and leaves it: the listing takes nothing at that
/// call, and at the call that gives what was read again it has seen everything.
#[test]
fn a_row_left_by_the_last_page_of_a_full_listing() {
    let seen = [root(), directory("G", "R", "G"), directory("D", "G", "D"), file("C", "X", "c", "1"), file("K", "X", "kept", "1"), directory("S", "X", "s"), file("F", "S", "f", "1")];
    let after_call = || {
        let mut store = store_of_tree(false);
        store.item_store().begin_full_listing().unwrap();
        confirm_rename_of_x(&mut store, "x2");
        assert_eq!(give(&mut store, &seen, 0, &next_link("next-1")), left(&[], CONFIRMED, None));
        assert_eq!(give(&mut store, &[x_moved_to_root()], 0, &delta_link("link-2")), left(&["X"], CONFIRMED, None));
        store
    };
    const AT_ROOT: &str = "C=X/c D=G/D F=S/f G=R/G K=X/kept S=X/s X=R/x2";
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "read again");
    let mut resumed = after_call();
    assert_eq!(under_way(&mut resumed).and_then(|listing| listing.next_link), Some("next-1".to_owned()));
    assert_eq!(give(&mut resumed, &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "the last page asked again");
}

/// A full listing ends without having seen `X` or anything below it. The sweep leaves `X`
/// and stops there: what is below it keeps its version until `X` was read. Read again and
/// there, `X` is seen, and the listing's end takes what it did not see below it; read
/// again and gone, the removal takes it. The last page asked again leaves `X` again: the
/// sweep is bounded by the count the listing began at.
#[test]
fn a_row_left_by_the_sweep_of_a_full_listing_and_what_is_below_it() {
    let last_page = [root(), directory("G", "R", "G"), directory("D", "G", "D")];
    let after_call = || {
        let mut store = store_of_tree(false);
        store.item_store().begin_full_listing().unwrap();
        confirm_rename_of_x(&mut store, "x2");
        assert_eq!(give(&mut store, &last_page, 0, &delta_link("link-2")), left(&["X"], CONFIRMED, None));
        store
    };
    assert_eq!(give(&mut after_call(), &[directory("X", "D", "x2")], 1, &delta_link("link-2")), left(&[], "D=G/D G=R/G K=- X=D/x2", Some("link-2")), "read again: there");
    assert_eq!(give(&mut after_call(), &[gone("X")], 1, &delta_link("link-2")), left(&[], "D=G/D G=R/G K=- X=-", Some("link-2")), "read again: gone");
    assert_eq!(give(&mut after_call(), &last_page, 1, &delta_link("link-2")), left(&["X"], CONFIRMED, None), "the last page asked again");
}

/// A full listing ends without having seen `X`, and saw what is below it: only `X` is
/// left, and nothing is taken.
#[test]
fn a_row_left_by_the_sweep_of_a_full_listing_that_saw_what_is_below_it() {
    let seen = [root(), directory("G", "R", "G"), directory("D", "G", "D"), file("C", "X", "c", "1"), file("K", "X", "kept", "1"), directory("S", "X", "s"), file("F", "S", "f", "1")];
    let mut store = store_of_tree(false);
    store.item_store().begin_full_listing().unwrap();
    confirm_rename_of_x(&mut store, "x2");
    assert_eq!(give(&mut store, &seen, 0, &delta_link("link-2")), left(&["X"], CONFIRMED, None));
    assert_eq!(give(&mut store, &[directory("X", "D", "x2")], 1, &delta_link("link-2")), left(&[], CONFIRMED, Some("link-2")), "read again");
}

/// The page that leaves `X` brings the root of the drive, whose id a row made at a first
/// mention had: that row goes and what OneDrive has in it is in row 1, `X` too, at the
/// place and under the name that were confirmed.
#[test]
fn a_row_left_and_the_root_listed_after_it() {
    let after_call = || {
        let mut store = store_of_drive();
        assert_eq!(give(&mut store, &[directory("X", "R", "x"), file("C", "X", "c", "1")], 0, &next_link("next-1")), left(&[], "C=X/c R=- X=R/x", None));
        let (x, first_mention) = (listed(&mut store, "X").local_item_id, listed(&mut store, "R").local_item_id);
        assert_ne!(first_mention, ROOT);
        confirm(&mut store, x, version(first_mention, "x2", None), Some("e-X-ours"));
        assert_eq!(give(&mut store, &[root(), directory("X", "R", "other")], 0, &next_link("next-2")), left(&["X"], "C=X/c X=R/x2", None));
        assert_eq!(onedrive_place(&mut store, "X"), Some(place(ROOT, "x2")));
        assert_eq!(under_way(&mut store).and_then(|listing| listing.next_link), Some("next-1".to_owned()));
        store
    };
    assert_eq!(give(&mut after_call(), &[directory("X", "R", "other")], 1, &next_link("next-2")), left(&[], "C=X/c X=R/other", None), "read again");
    assert_eq!(give(&mut after_call(), &[root(), directory("X", "R", "other")], 1, &next_link("next-2")), left(&[], "C=X/c X=R/other", None), "the page asked again");
}

/// The page that leaves `X` says of the items in it: one changed, one new, one moved out.
/// Each is its own item's statement and is written; the row of `X` is their directory as
/// before.
#[test]
fn a_row_left_and_its_children_changed_made_and_moved_out() {
    const AT_ROOT: &str = "C=X/c D=G/D F=S/f G=R/G K=R/kept N=X/n S=X/s X=R/x2";
    let page = [x_moved_to_root(), file("C", "X", "c", "22"), file("N", "X", "n", "1"), file("K", "R", "kept", "1")];
    let after_call = || {
        let mut store = store_of_confirmed_x(false);
        assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], "C=X/c D=G/D F=S/f G=R/G K=R/kept N=X/n S=X/s X=D/x2", Some("link-1")));
        assert_eq!(listed(&mut store, "C").onedrive.and_then(|onedrive| onedrive.content), Some(content("22")));
        store
    };
    assert_eq!(give(&mut after_call(), &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "read again");
    assert_eq!(give(&mut after_call(), &page, 1, &delta_link("link-2")), left(&[], AT_ROOT, Some("link-2")), "the page asked again");
}

/// The page that leaves `X` names a directory for the first time, as the parent of a new
/// item: its row is made with the binding alone and stays while that item names it. A
/// later page says it is in `X`.
#[test]
fn a_row_left_and_a_parent_first_mentioned_below_it() {
    let page = [x_moved_to_root(), file("N", "Q", "n", "1")];
    let mut store = store_of_confirmed_x(false);
    assert_eq!(give(&mut store, &page, 0, &next_link("next-1")), left(&["X"], "C=X/c D=G/D F=S/f G=R/G K=X/kept N=Q/n Q=- S=X/s X=D/x2", Some("link-1")));
    assert_eq!(give(&mut store, &[x_moved_to_root()], 1, &next_link("next-1")), left(&[], "C=X/c D=G/D F=S/f G=R/G K=X/kept N=Q/n Q=- S=X/s X=R/x2", Some("link-1")), "read again");
    assert_eq!(give(&mut store, &[directory("Q", "X", "q")], 1, &delta_link("link-2")), left(&[], "C=X/c D=G/D F=S/f G=R/G K=X/kept N=Q/n Q=X/q S=X/s X=R/x2", Some("link-2")));
}

/// The page that leaves `X` says it is in a directory no row knows: no row is made for
/// that directory by a change that is not written. The call that writes it makes the row.
#[test]
fn a_row_left_makes_no_row_for_the_parent_the_page_names() {
    let page = [directory("X", "P", "x2")];
    let mut store = store_of_confirmed_x(false);
    assert_eq!(give(&mut store, &page, 0, &delta_link("link-2")), left(&["X"], CONFIRMED, Some("link-1")));
    assert_eq!(bound(&mut store, "P"), None);
    assert_eq!(give(&mut store, &page, 1, &delta_link("link-2")), left(&[], "C=X/c D=G/D F=S/f G=R/G K=X/kept P=- S=X/s X=P/x2", Some("link-2")), "read again");
}

/// Between the call and what was read again, `X`, which the folder shows, gives its
/// binding up: the row that took it has the stamp, the version, the items below and the
/// removed directory as its parent, and what was read again is written to it.
#[test]
fn a_row_left_gives_its_binding_up_before_it_is_read_again() {
    let mut store = store_of_confirmed_x(true);
    let shown = listed(&mut store, "X").local_item_id;
    assert_eq!(give(&mut store, &[x_moved_to_root(), gone("D")], 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2", Some("link-1")));
    let taker = store.item_store().give_up_binding(shown, false).unwrap();
    assert_eq!((listed(&mut store, "X").local_item_id, listed(&mut store, "X").confirmed_at_count), (taker, 1));
    assert_eq!(said(&mut store), "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2");
    assert_eq!(give(&mut store, &[x_moved_to_root(), gone("D")], 0, &delta_link("link-2")).0, ["X"], "the page given again is left again");
    assert_eq!(give(&mut store, &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], "C=X/c F=S/f G=R/G K=X/kept S=X/s X=R/x2", Some("link-2")), "read again");
    assert_eq!(item(&mut store, shown).onedrive_item, None);
}

/// Between the call and what was read again, `X`, which the folder does not show, is
/// merged with a new directory of the folder: by `merge_items`, and by `bind_item` of
/// its reference. The row that took the binding is the one read again.
#[test]
fn a_row_left_is_merged_with_a_new_item_before_it_is_read_again() {
    for by_binding in [false, true] {
        let mut store = store_of_confirmed_x(false);
        let listed_row = listed(&mut store, "X").local_item_id;
        assert_eq!(give(&mut store, &[x_moved_to_root(), gone("D")], 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2", Some("link-1")));
        let made = make_directory(&mut store, ROOT, "made");
        if by_binding {
            store.item_store().bind_item(made, &reference("X")).unwrap();
        } else {
            store.item_store().merge_items(made, listed_row).unwrap();
        }
        assert_eq!((listed(&mut store, "X").local_item_id, listed(&mut store, "X").confirmed_at_count), (made, 1), "by_binding: {by_binding}");
        assert_eq!(said(&mut store), "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2");
        assert_eq!(give(&mut store, &[gone("X")], 0, &delta_link("link-2")).0, ["X"], "a page fetched before the confirmation is left still");
        assert_eq!(give(&mut store, &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], "C=X/c F=S/f G=R/G K=X/kept S=X/s X=R/x2", Some("link-2")), "read again, by_binding: {by_binding}");
    }
}

/// The row left is the row of a confirmed removal, which a page fetched before says
/// OneDrive has, in a directory the same page says is gone. The row stays through the
/// deletion of rows with a binding alone at the call's end, stamped as it is; the removed
/// directory, which nothing names, goes. Read again and gone, the removal takes what the
/// rows said was below it; the row itself goes when a listing ends that began after its
/// stamp.
#[test]
fn a_row_left_is_the_row_of_a_confirmed_removal() {
    let mut store = store_of_tree(false);
    let x = listed(&mut store, "X").local_item_id;
    assert_eq!(confirm(&mut store, x, None, None), 1);
    assert_eq!(give(&mut store, &[directory("X", "D", "x"), gone("D")], 0, &delta_link("link-2")), left(&["X"], "C=X/c F=S/f G=R/G K=X/kept S=X/s X=-", Some("link-1")));
    assert_eq!(give(&mut store, &[gone("X")], 1, &delta_link("link-2")), left(&[], "G=R/G K=- X=-", Some("link-2")), "read again");
    assert_eq!(give(&mut store, &[], 1, &delta_link("link-3")), left(&[], "G=R/G K=-", Some("link-3")), "a listing that began after the removal ends");
}

/// What was read again is outdated again: a second confirmation came between. The call
/// leaves everything as the first did, the removed directory's row too, and writes no
/// link; the next reading is written.
#[test]
fn a_row_left_is_read_again_and_left_again() {
    let mut store = store_of_confirmed_x(false);
    assert_eq!(give(&mut store, &[x_moved_to_root(), gone("D")], 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2", Some("link-1")));
    assert_eq!(confirm_rename_of_x(&mut store, "x3"), 2);
    assert_eq!(give(&mut store, &[x_moved_to_root()], 1, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x3", Some("link-1")), "read again, and left again");
    assert_eq!(give(&mut store, &[directory("X", "R", "x3")], 2, &delta_link("link-2")), left(&[], "C=X/c F=S/f G=R/G K=X/kept S=X/s X=R/x3", Some("link-2")), "read again");
}

/// The step that renamed `X` wrote that the two sides agree it is in `D`. The row of the
/// removed directory then stays past the call that writes where `X` is, for the place
/// agreed on names it; it goes at the first call after the folder followed.
#[test]
fn the_row_of_the_removed_directory_stays_while_a_place_agreed_on_names_it() {
    let mut store = store_of_confirmed_x(false);
    agree(&mut store, "X");
    assert_eq!(give(&mut store, &[x_moved_to_root(), gone("D")], 0, &delta_link("link-2")), left(&["X"], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=D/x2", Some("link-1")));
    assert_eq!(give(&mut store, &[x_moved_to_root()], 1, &delta_link("link-2")), left(&[], "C=X/c D=- F=S/f G=R/G K=X/kept S=X/s X=R/x2", Some("link-2")), "read again");
    agree(&mut store, "X");
    assert_eq!(give(&mut store, &[], 1, &delta_link("link-3")), left(&[], "C=X/c F=S/f G=R/G K=X/kept S=X/s X=R/x2", Some("link-3")));
}
