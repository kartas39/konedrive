use super::*;
use crate::model::ItemKind;
use crate::sync_plan::tests::{id, listed, made_file, place, removed_here, root, synced_directory, synced_file, ROOT};

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
