//! A directory that was removed here: its removal is sent on a positive condition — all the
//! rows say OneDrive has below it was shown here and removed here by the user (the design's
//! §6, the cell "removed here, is there in OneDrive"; the brief's §8). Otherwise the removal
//! is of the folder alone. And a directory the folder cannot show, which is in the folder
//! only because it holds the user's own: nothing of its place is sent.

use konedrive_reason::Reason;

use super::*;

fn waits_for(number: i64) -> Answer {
    Answer::Wait(WaitReason::AnotherItem(id(number)))
}

fn forgets(given: &SyncPlanInput, number: i64) -> Answer {
    row_step(RowStep::ForgetSyncedVersion { item: versions(given, number) })
}

fn cannot_be_shown(mut given: SyncPlanInput, numbers: &[i64]) -> SyncPlanInput {
    given.folder_visibility.extend(numbers.iter().map(|number| (id(*number), FolderVisibility::CannotBeShown)));
    given
}

/// The cell as rewritten for the second fix round. OneDrive made a file in a directory that
/// was removed here, which the folder never showed: the removal is of the folder alone.
/// What was agreed on of the directory is forgotten, so that it is an item OneDrive has that
/// the folder does not show; the file the user did remove in it is a removal of its own;
/// and the new file waits for the directory, with which it is shown by the plans after.
#[test]
fn the_removal_of_a_directory_is_of_the_folder_alone_when_onedrive_has_in_it_what_the_folder_never_showed() {
    let given = input(vec![removed_here(synced_directory(2, ROOT, "d")), listed(3, ItemKind::File, id(2), "new.txt"), removed_here(synced_file(4, id(2), "a.txt"))]);
    assert_eq!(answer(&given, 2), forgets(&given, 2));
    assert_eq!(answer(&given, 3), waits_for(2), "it is shown once the directory is");
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 4) }), "what the user removed in it is a removal of its own");

    // The plan after it: the directory is an item of OneDrive the folder never showed.
    let given = input(vec![listed(2, ItemKind::Directory, ROOT, "d"), listed(3, ItemKind::File, id(2), "new.txt"), removed_here(synced_file(4, id(2), "a.txt"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 4) }));
}

/// The second review's M1, as rows: `d` cannot be shown and was kept in the folder for the
/// user's own; the plan itself took `f` off the folder (its row is that of an item the
/// folder never showed); the user removed their own and `d`. Nothing is removed from
/// OneDrive: the directory forgets, and both are items OneDrive has that the folder cannot
/// show.
#[test]
fn a_directory_the_folder_cannot_show_removed_here_is_not_removed_from_onedrive_with_what_the_plan_took_off_the_folder() {
    let given = cannot_be_shown(input(vec![removed_here(synced_directory(4, ROOT, "d")), listed(8, ItemKind::File, id(4), "f")]), &[4, 8]);
    assert_eq!(answer(&given, 4), forgets(&given, 4));
    assert_eq!(answer(&given, 8), Answer::Exclusion(ExclusionReason::CannotBeShown));
    let given = cannot_be_shown(input(vec![listed(4, ItemKind::Directory, ROOT, "d"), listed(8, ItemKind::File, id(4), "f")]), &[4, 8]);
    assert_eq!((answer(&given, 4), answer(&given, 8)), (Answer::Exclusion(ExclusionReason::CannotBeShown), Answer::Exclusion(ExclusionReason::CannotBeShown)));
}

/// Directories removed in one another, and OneDrive made a file in the inner one: neither
/// removal is sent. Both forget — the outer one first, the inner one in the plan after —,
/// and each file the user removed is a removal of its own where no directory above it is
/// still to be removed.
#[test]
fn directories_removed_in_one_another_are_both_of_the_folder_alone_when_the_inner_one_holds_what_was_never_shown() {
    let given = input(vec![
        removed_here(synced_directory(2, ROOT, "d")),
        removed_here(synced_directory(3, id(2), "e")),
        listed(4, ItemKind::File, id(3), "new.txt"),
        removed_here(synced_file(5, id(2), "a.txt")),
        removed_here(synced_file(6, id(3), "b.txt")),
    ]);
    assert_eq!(answer(&given, 2), forgets(&given, 2));
    assert_eq!(answer(&given, 3), forgets(&given, 3), "the inner one is decided by what is below it");
    assert_eq!(answer(&given, 5), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 5) }));
    assert_eq!(answer(&given, 6), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 6) }));
    assert_eq!(answer(&given, 4), waits_for(3));
}

/// The outer directory holds what was never shown, the inner one does not: the inner one's
/// removal is sent, one removal for all that was removed with it, beside the outer one's
/// forgetting.
#[test]
fn a_directory_removed_with_all_in_it_inside_one_that_is_of_the_folder_alone_is_one_removal() {
    let given = input(vec![
        removed_here(synced_directory(2, ROOT, "d")),
        listed(3, ItemKind::File, id(2), "new.txt"),
        removed_here(synced_directory(4, id(2), "e")),
        removed_here(synced_file(5, id(4), "b.txt")),
    ]);
    assert_eq!(answer(&given, 2), forgets(&given, 2));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 4), below: vec![versions(&given, 5)] }));
    assert_eq!(answer(&given, 5), waits_for(4));
}

/// The design's "a directory removed here, something changed below it in OneDrive: the
/// removal is sent and stands" (stage B's B4c), beside the rule: what was shown here and
/// removed here stays removed where OneDrive renamed it or moved it within the directory
/// since. A file's own removal is sent then too.
#[test]
fn the_removal_of_a_directory_stands_for_what_was_shown_and_removed_here_and_onedrive_moved() {
    let given = input(vec![
        removed_here(synced_directory(2, ROOT, "d")),
        removed_here(synced_file(3, id(2), "a.txt")),
        moved_in_onedrive(removed_here(synced_file(4, id(2), "b.txt")), id(2), "b2.txt"),
        moved_in_onedrive(removed_here(synced_file(5, ROOT, "g")), id(2), "g"),
    ]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 3), versions(&given, 4), versions(&given, 5)] }));
    assert_eq!((answer(&given, 3), answer(&given, 4), answer(&given, 5)), (waits_for(2), waits_for(2), waits_for(2)), "none comes back, none has a removal of its own");
}

/// The third review's S3: a content OneDrive changed is never removed with a directory. A
/// file that was removed here and whose content OneDrive changed since comes back by the
/// file's own cell; below a directory that was removed here — it was in it, or OneDrive
/// moved it there — it makes that removal one of the folder alone, as what the folder
/// never showed does. The directory forgets and is shown again by the plan after, the
/// file with OneDrive's content in it; what else the user removed in it is a removal of
/// its own.
#[test]
fn the_removal_of_a_directory_is_of_the_folder_alone_when_onedrive_changed_the_content_of_a_file_removed_here_below_it() {
    let changed_in_it = || removed_here(with_content_in_onedrive(synced_file(3, id(2), "a.txt"), hashed("theirs")));
    let moved_into_it = || moved_in_onedrive(removed_here(with_content_in_onedrive(synced_file(3, ROOT, "g"), hashed("theirs"))), id(2), "g");
    for changed in [changed_in_it, moved_into_it] {
        let given = input(vec![removed_here(synced_directory(2, ROOT, "d")), changed(), removed_here(synced_file(4, id(2), "b.txt"))]);
        assert_eq!(answer(&given, 2), forgets(&given, 2));
        assert_eq!(answer(&given, 3), waits_for(2), "it comes back once the directory is shown again");
        assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 4) }));

        // The plans after it: the directory is an item of OneDrive the folder never showed,
        // and then one it shows.
        let given = input(vec![listed(2, ItemKind::Directory, ROOT, "d"), changed()]);
        assert_eq!((answer(&given, 2), answer(&given, 3)), (folder_step(FolderStep::ShowItem { item: versions(&given, 2) }), waits_for(2)));
        let given = input(vec![synced_directory(2, ROOT, "d"), changed()]);
        assert_eq!(answer(&given, 3), folder_step(FolderStep::ShowItem { item: versions(&given, 3) }));
    }
    // Inside a directory removed in the one that was removed: both are of the folder alone.
    let given = input(vec![removed_here(synced_directory(2, ROOT, "d")), removed_here(synced_directory(5, id(2), "e")), removed_here(with_content_in_onedrive(synced_file(3, id(5), "a.txt"), hashed("theirs")))]);
    assert_eq!((answer(&given, 2), answer(&given, 5), answer(&given, 3)), (forgets(&given, 2), forgets(&given, 5), waits_for(5)));
}

/// While a listing is under way a row may yet be told to be elsewhere: nothing is decided
/// of the removal, also not that it is of the folder alone.
#[test]
fn nothing_is_forgotten_of_a_removed_directory_while_a_listing_is_under_way() {
    let mut given = input(vec![removed_here(synced_directory(2, ROOT, "d")), listed(3, ItemKind::File, id(2), "new.txt")]);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
}

/// What holds a removal back comes before what makes it one of the folder alone: an item
/// below with a step running, and one the folder shows.
#[test]
fn a_removed_directory_waits_for_what_is_held_back_below_it_before_it_forgets() {
    let rows = || vec![removed_here(synced_directory(2, ROOT, "d")), listed(3, ItemKind::File, id(2), "new.txt"), removed_here(synced_file(4, id(2), "a.txt"))];
    let mut given = input(rows());
    given.items_with_running_step = [id(4)].into();
    assert_eq!(answer(&given, 2), waits_for(4));
    let mut given = input(rows());
    given.items_with_running_step = [id(3)].into();
    assert_eq!(answer(&given, 2), waits_for(3));
}

/// The second review's M1, the rename: a directory the folder cannot show is in the folder
/// only because it holds the user's own. Renamed here, its move is not sent: it waits for
/// what the folder shows in it, which waits for the user, and goes once that is out of it.
#[test]
fn a_directory_the_folder_cannot_show_that_was_renamed_here_is_not_sent() {
    let given = cannot_be_shown(input(vec![moved_here(synced_directory(4, ROOT, "d"), ROOT, "d-mine"), made_file(11, id(4), "mine.txt")]), &[4]);
    assert_eq!(answer(&given, 4), waits_for(11));
    assert_eq!(answer(&given, 11), Answer::Wait(WaitReason::User(Reason::Parent)));
    // The user's own moved out of it: it goes here, and nothing of it is sent.
    let given = cannot_be_shown(input(vec![moved_here(synced_directory(4, ROOT, "d"), ROOT, "d-mine")]), &[4]);
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RemoveItem { item: versions(&given, 4) }));
    // Moved on both sides: the same.
    let given = cannot_be_shown(input(vec![synced_directory(2, ROOT, "a"), moved_in_onedrive(moved_here(synced_directory(4, ROOT, "d"), ROOT, "d-mine"), id(2), "d")]), &[4]);
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RemoveItem { item: versions(&given, 4) }));
}
