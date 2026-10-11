//! The plan for an item an upload of this folder left in OneDrive with other content than
//! was sent ([`SyncPlanInput::items_left_by_uploads`]): the row a listing made of it is
//! not shown, no file is merged with it or renamed to a conflict copy for it, its name is
//! not taken to the file it was left by, and a directory is removed with it.

use super::*;

fn send_new_file(given: &SyncPlanInput, number: i64, name: &str) -> Answer {
    onedrive_step(OneDriveStep::SendNewFile { item: versions(given, number), parent: reference("root"), name: name.into() })
}

/// The file `new.txt` (2), and what its upload left under its name (3).
#[test]
fn what_an_upload_left_is_not_shown_and_its_file_is_sent_again_under_its_name() {
    let mut given = input(vec![made_file(2, ROOT, "new.txt"), listed(3, ItemKind::File, ROOT, "new.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::HashLocalContent { item: versions(&given, 2) }), "not told: an item another made, perhaps the same file");
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))));

    given.items_left_by_uploads = [(id(3), Some(id(2)))].into();
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::LeftByUpload));
    assert_eq!(answer(&given, 2), send_new_file(&given, 2, "new.txt"));
    // A listing under way may say nothing else of it.
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::LeftByUpload));
}

/// Another file at the name of what was left: the file whose upload left it (2) was renamed
/// here, and a file was made at its name (4). That file is neither merged with what was
/// left nor renamed for it: it waits for the file whose sending removes it; where that
/// file's row went, for the removal the daemon sends by itself; and one that is left here
/// is left here.
#[test]
fn another_file_at_the_name_of_what_an_upload_left_waits_for_its_removal() {
    let mut given = input(vec![made_file(2, ROOT, "renamed.txt"), listed(3, ItemKind::File, ROOT, "new.txt"), made_file(4, ROOT, "new.txt")]);
    given.items_left_by_uploads = [(id(3), Some(id(2)))].into();
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::AnotherItem(id(2))));
    assert_eq!(answer(&given, 2), send_new_file(&given, 2, "renamed.txt"));
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::LeftByUpload));

    given.items_left_by_uploads = [(id(3), None)].into();
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::RunningStep), "the daemon's own removal is not settled");

    given.ignored_items = [id(4)].into();
    assert_eq!(answer(&given, 4), Answer::Exclusion(ExclusionReason::NameIgnored));
}

/// The review's S2: what the file's own upload left is passed over where the holder of the
/// name is found, so that another item OneDrive has under a name of that fold is asked.
#[test]
fn another_item_of_the_name_is_asked_past_what_the_files_own_upload_left() {
    let mut given = input(vec![listed(2, ItemKind::File, ROOT, "new.txt"), made_file(3, ROOT, "new.txt"), listed(4, ItemKind::File, ROOT, "NEW.txt")]);
    given.items_left_by_uploads = [(id(2), Some(id(3)))].into();
    assert_eq!(answer(&given, 3), row_step(RowStep::HashLocalContent { item: versions(&given, 3) }), "whether it and the other item are one file");
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::LeftByUpload));
}

/// A directory removed here (2) with a file removed here in it (3) and what an upload left
/// in it (4): the removal is sent, with the file; what was left goes with the directory
/// and waits for nothing. Not told, the removal would be of the folder alone.
#[test]
fn a_directory_is_removed_with_what_an_upload_left_in_it() {
    let mut given = input(vec![removed_here(synced_directory(2, ROOT, "d")), removed_here(synced_file(3, id(2), "f")), listed(4, ItemKind::File, id(2), "left.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::ForgetSyncedVersion { item: versions(&given, 2) }), "not told: what the folder never showed");

    given.items_left_by_uploads = [(id(4), None)].into();
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 3)] }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))));
    assert_eq!(answer(&given, 4), Answer::Exclusion(ExclusionReason::LeftByUpload));
}
