//! The table "its place": a test per cell, and per row P1–P11 of the map.

use konedrive_reason::Reason;

use super::*;

// -- same here, moved in OneDrive

/// P1. Today: `a_file_that_moved_takes_its_downloaded_copy_along`
/// (`fuse/cycle_step/cycle_actions/tests.rs`), `a_downloaded_copy_moves_with_its_file`
/// (`fuse/cycle_step/tests.rs`). The step is the one of P2: whether a copy moves along is
/// read off the row at its commit.
#[test]
fn a_file_on_disk_that_onedrive_moved_is_moved_here() {
    let given = input(vec![synced_directory(3, ROOT, "d"), moved_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), id(3), "b.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
}

/// P2. Today: `a_file_that_moved_and_is_not_downloaded_moves_nothing_on_disk`
/// (`fuse/cycle_step/cycle_actions/tests.rs`).
#[test]
fn a_file_not_on_disk_that_onedrive_moved_is_moved_here() {
    let given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
}

/// P3. Today: `a_directory_that_moved_has_its_rows_rebased_before_it_moves`
/// (`fuse/cycle_step/cycle_actions/tests.rs`),
/// `a_directory_of_the_backing_directory_moves_with_its_directory_of_onedrive`
/// (`fuse/cycle_step/tests.rs`). Nothing is rebased now: what is in the directory names it
/// by its id.
#[test]
fn a_directory_that_onedrive_moved_is_moved_here() {
    let given = input(vec![synced_directory(3, ROOT, "d"), moved_in_onedrive(synced_directory(2, ROOT, "e"), id(3), "e"), synced_file(4, id(2), "a.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 4), Answer::Nothing);
}

/// P1 to P3, between the item and its neighbours. New as a rule: today the directories
/// above are made on demand, and a file that comes to where another entry stays is not
/// moved (`a_file_that_moved_to_where_another_entry_stays_is_not_moved_onto_it`,
/// `fuse/cycle_step/cycle_actions/tests.rs`). Here it waits for the directory to be shown,
/// or for the item that has the name; the order between them is decided between items.
#[test]
fn an_item_onedrive_moved_waits_for_its_directory_and_for_its_name_here() {
    let given = input(vec![
        listed(3, ItemKind::Directory, ROOT, "d"),
        moved_in_onedrive(synced_file(2, ROOT, "a.txt"), id(3), "a.txt"),
        removed_in_onedrive(synced_file(5, ROOT, "taken.txt")),
        moved_in_onedrive(synced_file(4, ROOT, "b.txt"), ROOT, "taken.txt"),
    ]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::AnotherItem(id(5))));
}

/// P6. Today: `an_id_placed_only_in_the_store_vanished`
/// (`fuse/cycle_step/onedrive_change/tests.rs`): it goes here as if removed. Also an item
/// whose placement alone changed, where it is, and one OneDrive gave a name that is no
/// name of a directory entry. Nothing is done while a listing is under way.
#[test]
fn an_item_onedrive_has_where_the_folder_cannot_show_it_goes_here_as_if_removed() {
    let mut given = input(vec![
        synced_directory(3, ROOT, "vault"),
        moved_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), id(3), "a.txt"),
        synced_file(4, ROOT, "b.txt"),
        moved_in_onedrive(synced_file(5, ROOT, "c.txt"), ROOT, "c/d"),
        moved_in_onedrive(synced_directory(6, ROOT, "e"), id(3), "e"),
        synced_file(7, id(6), "f.txt"),
    ]);
    for cannot_be_shown in [2, 4, 6, 7] {
        given.folder_visibility.insert(id(cannot_be_shown), FolderVisibility::CannotBeShown);
    }
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RemoveItem { item: versions(&given, 4) }), "its placement alone changed");
    assert_eq!(answer(&given, 5), folder_step(FolderStep::RemoveItem { item: versions(&given, 5) }), "no name of a directory entry");
    assert_eq!(answer(&given, 6), Answer::Wait(WaitReason::AnotherItem(id(7))), "a directory goes after what the folder shows in it");
    assert_eq!(answer(&given, 7), folder_step(FolderStep::RemoveItem { item: versions(&given, 7) }));

    given.is_listing_under_way = true;
    for item in [2, 4, 5, 6, 7] {
        assert_eq!(answer(&given, item), Answer::Wait(WaitReason::Listing), "item {item}");
    }
}

/// The brief's rule for an item whose side in OneDrive cannot be judged yet.
#[test]
fn an_item_both_sides_have_that_cannot_be_judged_yet_waits_for_the_listing_to_end() {
    let mut given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), with_content_here(synced_file(3, ROOT, "c.txt"), written_here())]);
    given.folder_visibility.insert(id(2), FolderVisibility::NotKnownYet);
    given.folder_visibility.insert(id(3), FolderVisibility::NotKnownYet);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing));
    // With no listing under way it is a fault of the rows (the brief's §5, 6).
    given.is_listing_under_way = false;
    assert_eq!(answer(&given, 2), Answer::OtherFault(PlanFault::FullListingNeeded(id(2))));
}

// -- moved here, same in OneDrive

/// P7. Today: `edits_and_renames_go_up_guarded` and
/// `a_file_that_is_not_downloaded_is_moved_and_deleted_in_onedrive`
/// (`upload/tests/row_kinds.rs`); the scenarios `renamed_file_keeps_its_item`,
/// `moved_file_keeps_its_item`, `renamed_and_moved_directory_keeps_its_items`. The guard is
/// the eTag the step carries with the versions.
#[test]
fn a_move_made_here_is_sent() {
    let given = input(vec![
        synced_directory(3, ROOT, "d"),
        moved_here(synced_file(2, ROOT, "a.txt"), id(3), "b.txt"),
        moved_here(on_disk(synced_file(4, ROOT, "c.txt")), ROOT, "C.txt"),
        moved_here(synced_directory(5, ROOT, "e"), id(3), "e"),
    ]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("I3"), name: "b.txt".into() }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "C.txt".into() }));
    assert_eq!(answer(&given, 5), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 5), parent: reference("I3"), name: "e".into() }));
}

/// P7, the map's "a move into a directory being made" (`T/outbox/pick.rs:204-222`). Today:
/// `a_mkdir_at_the_end_of_the_queue_runs_first` (`outbox/pick/tests.rs`).
#[test]
fn a_move_made_here_waits_for_its_directory_to_be_in_onedrive() {
    let given = input(vec![made_directory(3, ROOT, "d"), moved_here(synced_file(2, ROOT, "a.txt"), id(3), "a.txt")]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))));
}

/// P11. Today: `conflicts_keep_both_and_the_first_rename_wins` (`upload/tests/conflicts.rs`,
/// "rename × edit there"): the move is sent with the fresh eTag, which is the row's. The
/// plan made after it takes OneDrive's content.
#[test]
fn a_move_made_here_is_sent_when_onedrive_changed_the_content() {
    let given = input(vec![with_content_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), hashed("theirs"))]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "b.txt".into() }));
    assert_eq!(versions(&given, 2).onedrive_etag.as_deref(), Some("e-changed"));
}

// -- moved on both sides

/// P8. Today: `every_crash_point_is_replayed_to_the_same_end` (`upload/tests/row_kinds.rs`,
/// a replayed move that is already there). Nothing is sent now: the rows say it.
#[test]
fn an_item_moved_to_one_place_on_both_sides_agrees() {
    let given = input(vec![moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), ROOT, "b.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnPlace { item: versions(&given, 2) }));
}

/// P9. Today: `conflicts_keep_both_and_the_first_rename_wins` (`upload/tests/conflicts.rs`,
/// "rename × rename there") and
/// `a_renamed_file_that_is_not_downloaded_follows_an_earlier_rename_in_onedrive`.
#[test]
fn an_item_moved_to_different_places_follows_onedrive_where_the_folder_can() {
    let given = input(vec![
        moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "mine.txt"), ROOT, "theirs.txt"),
        moved_in_onedrive(moved_here(synced_directory(3, ROOT, "d"), ROOT, "mine"), ROOT, "theirs"),
    ]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
}

/// P10. Today:
/// `a_renamed_file_that_is_not_downloaded_keeps_its_name_where_onedrives_name_is_taken_on_disk`
/// (`upload/tests/conflicts.rs`): where the folder cannot follow, the local place stands
/// and is sent. The three ways it cannot: the name is taken here, the folder cannot show
/// the item where OneDrive has it, the name cannot be held.
#[test]
fn an_item_moved_to_different_places_keeps_its_place_here_where_the_folder_cannot_follow() {
    let mut given = input(vec![
        made_file(3, ROOT, "theirs.txt"),
        moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "mine.txt"), ROOT, "theirs.txt"),
        synced_directory(5, ROOT, "vault"),
        moved_in_onedrive(moved_here(synced_file(4, ROOT, "b.txt"), ROOT, "mine-b.txt"), id(5), "b.txt"),
        moved_in_onedrive(moved_here(synced_file(6, ROOT, "c.txt"), ROOT, "mine-c.txt"), ROOT, "c/d"),
    ]);
    given.folder_visibility.insert(id(4), FolderVisibility::CannotBeShown);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "mine.txt".into() }), "the name is taken here");
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "mine-b.txt".into() }), "the folder cannot show it there");
    assert_eq!(answer(&given, 6), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 6), parent: reference("root"), name: "mine-c.txt".into() }), "no name of a directory entry");

    // Whether the folder can show it there is not acted on while a listing is under way.
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::Listing));
}

/// P9 and P10, between the item and its directory: OneDrive's directory is not shown here
/// yet, and will be. New: it waits for that directory, and follows then.
#[test]
fn an_item_moved_to_different_places_waits_for_onedrives_directory_to_be_shown() {
    let given = input(vec![listed(3, ItemKind::Directory, ROOT, "d"), moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "mine.txt"), id(3), "a.txt")]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))));
}

/// New: an item the folder shows and OneDrive has in one row, with nothing agreed on (two
/// new items that were merged; an item the start-up check bound by its mark). At one place
/// the two agree; at two, OneDrive's stands, as for an item moved on both sides.
#[test]
fn an_item_both_sides_have_with_nothing_agreed_on_agrees_on_one_place_and_follows_onedrive_to_another() {
    let mut at_one_place = synced_directory(2, ROOT, "d");
    at_one_place.synced_place = None;
    let mut at_two_places = moved_in_onedrive(synced_directory(3, ROOT, "e"), ROOT, "f");
    at_two_places.synced_place = None;
    let given = input(vec![at_one_place, at_two_places]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnPlace { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
}

/// The design's "a name OneDrive cannot hold is not sent and not altered": also the name a
/// move gives. Today: `a_name_onedrive_refuses_or_a_file_too_large_waits_for_the_user`
/// (`fuse/detections/entry_created/tests.rs`) is of a new item; a rename to such a name is
/// blocked by the worker (`upload/steps/shared.rs:26-34`).
#[test]
fn a_move_to_a_name_onedrive_cannot_hold_waits_for_the_user() {
    let mut given = input(vec![moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "a?.txt")]);
    given.name_refusals.insert(id(2), Reason::NameCharacters);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::NameCharacters)));
}
