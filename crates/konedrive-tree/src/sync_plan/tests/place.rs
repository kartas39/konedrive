//! The table "its place" of `docs/design/sync.md` §6.3: a test per cell.

use konedrive_reason::Reason;

use super::*;

// -- same here, moved in OneDrive

/// The step is the one of a file not on disk: whether a copy moves along is read off the
/// row at its commit.
#[test]
fn a_file_on_disk_that_onedrive_moved_is_moved_here() {
    let given = input(vec![synced_directory(3, ROOT, "d"), moved_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), id(3), "b.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
}

#[test]
fn a_file_not_on_disk_that_onedrive_moved_is_moved_here() {
    let given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
}

/// Nothing below is written: what is in the directory names it by its id.
#[test]
fn a_directory_that_onedrive_moved_is_moved_here() {
    let given = input(vec![synced_directory(3, ROOT, "d"), moved_in_onedrive(synced_directory(2, ROOT, "e"), id(3), "e"), synced_file(4, id(2), "a.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 4), Answer::Nothing);
}

/// An item OneDrive moved, between it and its neighbours: it waits for the directory to be shown,
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

/// Moved in OneDrive to where the folder cannot show it: it goes here as if removed. Also an item
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

/// An item whose side in OneDrive cannot be judged yet waits for the listing under way.
#[test]
fn an_item_both_sides_have_that_cannot_be_judged_yet_waits_for_the_listing_to_end() {
    let mut given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), with_content_here(synced_file(3, ROOT, "c.txt"), written_here())]);
    given.folder_visibility.insert(id(2), FolderVisibility::NotKnownYet);
    given.folder_visibility.insert(id(3), FolderVisibility::NotKnownYet);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing));
    // With no listing under way it is a fault of the rows.
    given.is_listing_under_way = false;
    assert_eq!(answer(&given, 2), Answer::OtherFault(PlanFault::FullListingNeeded(id(2))));
}

// -- moved here, same in OneDrive

/// The scenarios `renamed_file_keeps_its_item`, `moved_file_keeps_its_item`,
/// `renamed_and_moved_directory_keeps_its_items`. The guard is the eTag the step carries
/// with the versions.
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

/// A move into a directory being made: the directory is made first.
#[test]
fn a_move_made_here_waits_for_its_directory_to_be_in_onedrive() {
    let given = input(vec![made_directory(3, ROOT, "d"), moved_here(synced_file(2, ROOT, "a.txt"), id(3), "a.txt")]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))));
}

/// Moved here, its content changed in OneDrive: the move is sent with the fresh eTag,
/// which is the row's. The
/// plan made after it takes OneDrive's content.
#[test]
fn a_move_made_here_is_sent_when_onedrive_changed_the_content() {
    let given = input(vec![with_content_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), hashed("theirs"))]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "b.txt".into() }));
    assert_eq!(versions(&given, 2).onedrive_etag.as_deref(), Some("e-changed"));
}

// -- moved on both sides

/// Moved to one place on both sides: nothing is sent, the rows say it.
#[test]
fn an_item_moved_to_one_place_on_both_sides_agrees() {
    let given = input(vec![moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), ROOT, "b.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnPlace { item: versions(&given, 2) }));
}

#[test]
fn an_item_moved_to_different_places_follows_onedrive_where_the_folder_can() {
    let given = input(vec![
        moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "mine.txt"), ROOT, "theirs.txt"),
        moved_in_onedrive(moved_here(synced_directory(3, ROOT, "d"), ROOT, "mine"), ROOT, "theirs"),
    ]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
}

/// Where the folder cannot follow, the local place stands
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

/// Moved on both sides, between the item and its directory: OneDrive's directory is not
/// shown here yet, and will be. It waits for that directory, and follows then.
#[test]
fn an_item_moved_to_different_places_waits_for_onedrives_directory_to_be_shown() {
    let given = input(vec![listed(3, ItemKind::Directory, ROOT, "d"), moved_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "mine.txt"), id(3), "a.txt")]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))));
}

/// An item the folder shows and OneDrive has in one row, with nothing agreed on: two new
/// items that were merged, or an entry the check at start bound by the mark it carries. At
/// one place the two agree — also where the names differ only as OneDrive compares them,
/// as two merged items' may: the folder then follows OneDrive's spelling.
///
/// At two places the binding was a guess (a copy of a marked file, or a hard link, made
/// while no daemon ran): no move is sent and the entry is not moved here, as nobody moved
/// anything. It gives its binding up: the entry is a new item of the folder, and OneDrive's
/// item is shown at its own place by the plans that follow. While a listing is under way
/// it waits.
#[test]
fn an_item_both_sides_have_with_nothing_agreed_on_agrees_on_one_place_and_gives_up_its_binding_at_another() {
    let nothing_agreed_on = |mut item: Item| {
        item.synced_place = None;
        item.synced_content = None;
        item
    };
    let rows = || {
        vec![
            nothing_agreed_on(synced_directory(2, ROOT, "d")),
            nothing_agreed_on(moved_in_onedrive(synced_directory(3, ROOT, "e"), ROOT, "f")),
            synced_directory(4, ROOT, "docs"),
            nothing_agreed_on(on_disk(moved_in_onedrive(synced_file(5, ROOT, "copy.txt"), id(4), "a.txt"))),
            nothing_agreed_on(moved_in_onedrive(synced_directory(6, ROOT, "Readme"), ROOT, "README")),
            // Where the folder could not follow: the name is taken here.
            nothing_agreed_on(moved_in_onedrive(synced_file(7, ROOT, "seven"), ROOT, "taken")),
            synced_file(8, ROOT, "taken"),
        ]
    };
    let given = input(rows());
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnPlace { item: versions(&given, 2) }));
    for guessed in [3, 5, 7] {
        assert_eq!(answer(&given, guessed), row_step(RowStep::GiveUpBinding { item: versions(&given, guessed) }), "item {guessed}");
    }
    assert_eq!(answer(&given, 6), folder_step(FolderStep::MoveItem { item: versions(&given, 6) }), "one name to OneDrive: its spelling stands");
    let mut given = input(rows());
    given.is_listing_under_way = true;
    for guessed in [3, 5, 7] {
        assert_eq!(answer(&given, guessed), Answer::Wait(WaitReason::Listing), "item {guessed}");
    }
}

/// What the folder shows in a directory that gives up its binding in
/// this plan is not sent into the item the binding was of. It waits for the directory:
/// the plans after send the directory as a new one, and then what is in it.
#[test]
fn what_the_folder_shows_in_a_directory_that_gives_up_a_guessed_binding_waits_for_it() {
    let mut copy = moved_in_onedrive(synced_directory(3, ROOT, "copy"), id(4), "docs");
    (copy.synced_place, copy.synced_content) = (None, None);
    let given = input(vec![copy, synced_directory(4, ROOT, "a"), made_file(5, id(3), "new.txt"), made_directory(6, id(3), "sub"), moved_here(synced_file(7, ROOT, "seven"), id(3), "seven")]);
    assert_eq!(answer(&given, 3), row_step(RowStep::GiveUpBinding { item: versions(&given, 3) }));
    for shown_in_it in [5, 6, 7] {
        assert_eq!(answer(&given, shown_in_it), Answer::Wait(WaitReason::AnotherItem(id(3))), "item {shown_in_it}");
    }
}

/// Found by the larger convergence run (seed 106, requests in flight): two directories
/// OneDrive removed, each holding the user's own; the folder shows the first in the second,
/// and the two sides agreed the second is in the first. Only what is sent into a
/// directory waits for it to give up its binding: a directory that is shown in it and
/// gives up its own binding does not, or the two would wait for each other without end.
#[test]
fn a_directory_shown_in_one_that_gives_up_its_binding_gives_up_its_own_without_waiting_for_it() {
    let given = input(vec![
        removed_in_onedrive(moved_here(synced_directory(3, ROOT, "g"), id(5), "b")),
        removed_in_onedrive(moved_here(synced_directory(5, id(3), "f"), ROOT, "B")),
        made_file(7, id(3), "own"),
        made_file(9, id(5), "e.txt"),
    ]);
    assert_eq!(make_sync_plan(&given).faults, [], "no item waits without end");
    assert_eq!(answer(&given, 3), row_step(RowStep::GiveUpBinding { item: versions(&given, 3) }));
    assert_eq!(answer(&given, 5), Answer::Wait(WaitReason::AnotherItem(id(3))), "what was agreed to be in it follows its binding first");
}

/// A name OneDrive cannot hold is not sent and not altered (`docs/design/sync.md` §6.5):
/// also the name a move gives.
#[test]
fn a_move_to_a_name_onedrive_cannot_hold_waits_for_the_user() {
    let mut given = input(vec![moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "a?.txt")]);
    given.name_refusals.insert(id(2), Reason::NameCharacters);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::NameCharacters)));
}
