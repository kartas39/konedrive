//! The plan for an item OneDrive has under a temporary name
//! ([`SyncPlanInput::temporary_names`]): the name a request of this folder gave it in the
//! middle of an exchange of names. The folder is never brought to that name, the item is
//! sent on to its place here, and the row a listing made of a new item under it is the
//! item itself.

use super::*;
use crate::TemporaryName;

fn swap_name(number: i64) -> String {
    format!(".konedrive-swap-{number}")
}

/// What the steps remember of the item `number` on its way to `toward`.
fn remembered(number: i64, toward: ItemPlace) -> (LocalItemId, TemporaryName) {
    (id(number), TemporaryName { name: swap_name(number).into(), toward })
}

fn send_move(given: &SyncPlanInput, number: i64, name: &str) -> Answer {
    onedrive_step(OneDriveStep::SendMove { item: versions(given, number), parent: reference("root"), name: name.into() })
}

/// The typed case of the place table (`LocalPlaceAvailability::IsTemporaryName`), a row per
/// way the rows can say it: moved on both sides, moved in OneDrive alone, and nothing
/// agreed on (the row was merged with the one a listing made). Without what the steps
/// remember, each is planned as any item OneDrive moved: that is what went wrong.
#[test]
fn an_item_under_its_temporary_name_is_sent_on_and_the_folder_is_not_moved_there() {
    let moved_on_both_sides = moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), ROOT, &swap_name(2));
    let moved_in_onedrive_alone = moved_in_onedrive(synced_file(3, ROOT, "c.txt"), ROOT, &swap_name(3));
    let mut merged = listed(4, ItemKind::Directory, ROOT, &swap_name(4));
    merged.local = Some(ItemVersion { place: Some(place(ROOT, "d")), content: None });
    merged.unsent_since = Some(4);
    let mut given = input(vec![moved_on_both_sides, moved_in_onedrive_alone, merged]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }), "not told: the folder follows OneDrive");
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
    assert_eq!(answer(&given, 4), row_step(RowStep::GiveUpBinding { item: versions(&given, 4) }), "not told: a binding nothing is agreed on of is a guess");

    given.temporary_names = [remembered(2, place(ROOT, "b.txt")), remembered(3, place(ROOT, "c.txt")), remembered(4, place(ROOT, "d"))].into();
    assert_eq!(answer(&given, 2), send_move(&given, 2, "b.txt"));
    assert_eq!(answer(&given, 3), send_move(&given, 3, "c.txt"));
    assert_eq!(answer(&given, 4), send_move(&given, 4, "d"));
}

/// The move owed to OneDrive from a row that is not unsent — moved in OneDrive alone, to the
/// temporary name — waits for its next try as any step towards OneDrive: the caller keeps
/// the try itself, the store having no stamp to keep it under, and tells it as one of what
/// is sent.
#[test]
fn a_move_owed_from_a_row_that_is_not_unsent_waits_until_its_next_try() {
    let mut given = input(vec![moved_in_onedrive(synced_file(3, ROOT, "c.txt"), ROOT, &swap_name(3))]);
    given.temporary_names = [remembered(3, place(ROOT, "c.txt"))].into();
    assert_eq!(versions(&given, 3).unsent_since, None);
    given.next_tries = [((id(3), TriedStep::TowardsOneDrive), NextTry { not_before: given.now + 30, reason: Reason::Network })].into();
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Time { until: given.now + 30, reason: Reason::Network }));
    given.now += 30;
    assert_eq!(answer(&given, 3), send_move(&given, 3, "c.txt"), "the time has come");
}

/// A temporary name the steps remember says something only while OneDrive has the item
/// under it ([`TemporaryName::is_at`]): in the directory the item was on its way to, under
/// that very name.
#[test]
fn a_temporary_name_onedrive_does_not_have_the_item_under_says_nothing() {
    let item = || moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), ROOT, "elsewhere.txt");
    let mut given = input(vec![synced_directory(5, ROOT, "d"), item()]);
    let follows = folder_step(FolderStep::MoveItem { item: versions(&given, 2) });
    for (what, temporary_name) in [
        ("another name", TemporaryName { name: swap_name(2).into(), toward: place(ROOT, "b.txt") }),
        ("the name in another directory", TemporaryName { name: "elsewhere.txt".into(), toward: place(id(5), "b.txt") }),
    ] {
        given.temporary_names = [(id(2), temporary_name)].into();
        assert_eq!(answer(&given, 2), follows, "{what}");
    }
    // And a name another item keeps is not this one's.
    given.temporary_names = [(id(5), TemporaryName { name: "elsewhere.txt".into(), toward: place(ROOT, "d") })].into();
    assert_eq!(answer(&given, 2), follows);
}

/// The exchange of names killed in the middle: `x` and `y` exchanged here, `x` was sent to
/// its temporary name, and the daemon died. The other item takes the name that is free
/// now; the one under its temporary name waits for it, and is then sent on.
#[test]
fn an_exchange_of_names_killed_in_the_middle_goes_on_from_the_temporary_name() {
    let first = moved_in_onedrive(moved_here(synced_file(2, ROOT, "x"), ROOT, "y"), ROOT, &swap_name(2));
    let second = moved_here(synced_file(3, ROOT, "y"), ROOT, "x");
    let mut given = input(vec![first.clone(), second]);
    given.temporary_names = [remembered(2, place(ROOT, "y"))].into();
    assert_eq!(answer(&given, 3), send_move(&given, 3, "x"));
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))), "its name is still the other's");
    // The other's move is confirmed.
    let mut given = input(vec![first, synced_file(3, ROOT, "x")]);
    given.temporary_names = [remembered(2, place(ROOT, "y"))].into();
    assert_eq!(answer(&given, 2), send_move(&given, 2, "y"));
    assert_eq!(answer(&given, 3), Answer::Nothing);
}

/// The file `x` moved here into a new directory named `x`: the directory is made under its
/// temporary name. The rows after that request, whose answer was lost: the directory (2)
/// with no binding, the file (3), and the row a listing made of the directory (4).
fn rows_of_a_directory_made_under_its_temporary_name(listed_kind: ItemKind) -> SyncPlanInput {
    let mut given = input(vec![made_directory(2, ROOT, "x"), moved_here(synced_file(3, ROOT, "x"), id(2), "x"), listed(4, listed_kind, ROOT, &swap_name(2))]);
    given.temporary_names = [remembered(2, place(ROOT, "x"))].into();
    given
}

/// The review's M1 and M1b (`LocalPlaceAvailability::IsTemporaryNameOf`): the row a listing
/// made under another item's temporary name is not shown; it is that item, and the two
/// are merged. Not told, the plan shows the listed row and makes the directory a second
/// time, under the next temporary name.
#[test]
fn the_row_listed_under_an_items_temporary_name_is_merged_with_the_item_and_never_shown() {
    let mut given = rows_of_a_directory_made_under_its_temporary_name(ItemKind::Directory);
    assert_eq!(answer(&given, 2), row_step(RowStep::MergeItems { local: versions(&given, 2), onedrive: versions(&given, 4) }));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::AnotherItem(id(2))));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))), "OneDrive does not have its directory yet");

    given.temporary_names.clear();
    assert_eq!(answer(&given, 4), folder_step(FolderStep::ShowItem { item: versions(&given, 4) }), "not told: an item another made");
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 2), parent: reference("root"), name: format!("{}-2", swap_name(2)).into() }), "not told: made again");
}

/// While a listing is under way the two are not merged yet, and the listed row is not
/// shown either; nor is anything planned for an item that is held back, which the listed
/// row goes on waiting for.
#[test]
fn the_merge_under_a_temporary_name_waits_for_a_listing_and_for_what_holds_the_item_back() {
    let mut given = rows_of_a_directory_made_under_its_temporary_name(ItemKind::Directory);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::AnotherItem(id(2))));

    let mut given = rows_of_a_directory_made_under_its_temporary_name(ItemKind::Directory);
    given.items_with_running_step = [id(2)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::RunningStep));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::AnotherItem(id(2))));
}

/// A listed row of another kind is not the item: it is not merged, and it is not shown
/// under the temporary name either.
#[test]
fn a_row_of_another_kind_under_an_items_temporary_name_is_neither_merged_nor_shown() {
    let given = rows_of_a_directory_made_under_its_temporary_name(ItemKind::File);
    assert!(!matches!(answer(&given, 2), Answer::Step(SyncStep::Row(RowStep::MergeItems { .. }))), "{:?}", answer(&given, 2));
    assert!(!matches!(answer(&given, 4), Answer::Step(_)), "{:?}", answer(&given, 4));
}

/// Found by the enumeration: an item that has a binding is another item of OneDrive than a
/// row a listing made, whatever temporary name it remembers. The listed row is shown as
/// any item another made; it does not wait for an item that has nothing to do, which the
/// plan answered as a wait without end.
#[test]
fn a_row_under_the_temporary_name_an_item_with_a_binding_remembers_is_shown() {
    let mut given = input(vec![synced_file(2, ROOT, "x"), listed(3, ItemKind::File, ROOT, &swap_name(2))]);
    given.temporary_names = [remembered(2, place(ROOT, "x"))].into();
    assert_eq!(answer(&given, 3), folder_step(FolderStep::ShowItem { item: versions(&given, 3) }));
    assert_eq!(answer(&given, 2), Answer::Nothing);
}
