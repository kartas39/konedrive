use super::*;
use crate::model::ItemKind;
use crate::sync_plan::tests::{id, listed, made_file, moved_here, place, removed_here, root, synced_directory, synced_file, ROOT};

#[test]
fn a_given_row_is_found_by_its_id_and_one_that_was_not_given_is_not() {
    let items = [root(), synced_file(2, ROOT, "a.txt")];
    let given = PlanItems::of(&items);
    assert_eq!(given.find(id(2)), Some(&items[1]));
    assert_eq!(given.find(ROOT), Some(&items[0]));
    assert_eq!(given.find(id(3)), None);
}

#[test]
fn the_item_the_folder_shows_at_a_place_is_found_by_the_name_as_it_was_given() {
    let items = [synced_file(2, ROOT, "a.txt"), listed(3, ItemKind::File, ROOT, "b.txt"), removed_here(synced_file(4, ROOT, "c.txt"))];
    let given = PlanItems::of(&items);
    assert_eq!(given.find_shown_at(&place(ROOT, "a.txt")), Some(id(2)));
    assert_eq!(given.find_shown_at(&place(ROOT, "A.txt")), None, "another case of a letter is another name");
    assert_eq!(given.find_shown_at(&place(id(2), "a.txt")), None, "another directory");
    assert_eq!(given.find_shown_at(&place(ROOT, "b.txt")), None, "OneDrive has it there, and the folder does not show it");
    assert_eq!(given.find_shown_at(&place(ROOT, "c.txt")), None, "removed here");
}

#[test]
fn of_what_the_folder_shows_in_a_directory_the_item_with_the_lowest_id_is_found() {
    let items = [synced_directory(2, ROOT, "d"), made_file(5, id(2), "b.txt"), synced_file(4, id(2), "a.txt"), listed(3, ItemKind::File, id(2), "c.txt"), synced_directory(6, ROOT, "e")];
    let given = PlanItems::of(&items);
    assert_eq!(given.find_first_shown_in(id(2)), Some(id(4)));
    assert_eq!(given.find_first_shown_in(id(6)), None);
    assert_eq!(given.find_first_shown_in(ROOT), Some(id(2)));
}

#[test]
fn the_items_of_a_directory_are_listed_on_each_side_all_of_them_and_by_a_folded_name() {
    use crate::folded_name::FoldedName;
    use std::ffi::OsStr;
    let items = [
        root(),
        synced_directory(2, ROOT, "d"),
        made_file(5, id(2), "A.txt"),
        synced_file(4, id(2), "a.txt"),
        listed(3, ItemKind::File, id(2), "B.txt"),
        removed_here(synced_file(6, id(2), "b.TXT")),
    ];
    let given = PlanItems::of(&items);
    assert_eq!(given.list_shown_in(id(2)), [id(4), id(5)]);
    assert_eq!(given.list_shown_in(id(4)), []);
    let (a, b) = (FoldedName::of(OsStr::new("a.TXT")), FoldedName::of(OsStr::new("b.txt")));
    assert_eq!(given.list_shown_with_name(id(2), &a), [id(4), id(5)]);
    assert_eq!(given.list_shown_with_name(id(2), &b), []);
    assert_eq!(given.list_shown_with_name(ROOT, &a), []);
    assert_eq!(given.list_in_onedrive_with_name(id(2), &a), [id(4)]);
    assert_eq!(given.list_in_onedrive_with_name(id(2), &b), [id(3), id(6)], "two rows of one name, as while a listing is written");
}

#[test]
fn what_is_in_a_directory_at_any_depth_is_found_on_each_side() {
    // The folder shows 4 in 3 in 2; OneDrive has 4 in 2, and 3 in the root.
    let items = [root(), synced_directory(2, ROOT, "d"), moved_here(synced_directory(3, ROOT, "e"), id(2), "e"), moved_here(synced_file(4, id(2), "a.txt"), id(3), "a.txt"), listed(5, ItemKind::File, id(3), "new.txt")];
    let given = PlanItems::of(&items);
    assert_eq!(given.list_below_in_onedrive(id(2)), [id(4)]);
    assert_eq!(given.list_below_in_onedrive(ROOT), [id(2), id(4), id(3), id(5)], "a directory before what is in it");
    assert!(given.is_shown_in(id(4), id(2)) && given.is_shown_in(id(4), id(3)) && given.is_shown_in(id(3), id(2)));
    assert!(!given.is_shown_in(id(2), id(3)) && !given.is_shown_in(id(2), id(2)) && !given.is_shown_in(id(5), id(3)));
    assert!(given.is_in_onedrive_in(id(4), id(2)) && given.is_in_onedrive_in(id(5), id(3)) && given.is_in_onedrive_in(id(5), ROOT));
    assert!(!given.is_in_onedrive_in(id(4), id(3)) && !given.is_in_onedrive_in(id(3), id(2)));
    // A directory whose row was not given ends the way up.
    assert!(!given.is_shown_in(id(4), id(9)));
}
