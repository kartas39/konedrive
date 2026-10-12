//! What the check of the backing directory at start could not decide
//! ([`SyncPlanInput::items_left_by_start_up_check`]): held back with what is in it and
//! below it, which the plan finds by the rows.

use super::*;

/// The second review's S5: the caller names the items the check left; what the folder
/// shows in one of them, or the two sides agreed is in it, at any depth, is held with it —
/// nothing of it is sent, a removal neither, and nothing is taken over for it.
#[test]
fn what_is_in_or_below_an_item_the_check_at_start_left_is_held_with_it() {
    let mut given = input(vec![
        synced_directory(2, ROOT, "d"),
        removed_here(synced_file(3, id(2), "removed.txt")),
        with_content_here(synced_file(4, id(2), "changed.txt"), written_here()),
        synced_directory(5, id(2), "e"),
        with_content_in_onedrive(synced_file(6, id(5), "theirs.txt"), hashed("theirs")),
        made_file(7, id(5), "new.txt"),
        removed_here(synced_file(8, ROOT, "beside.txt")),
    ]);
    given.items_left_by_start_up_check = [id(2)].into();
    for held in [3, 4, 6, 7] {
        assert_eq!(answer(&given, held), Answer::Wait(WaitReason::StartUpCheck), "item {held}");
    }
    assert_eq!(answer(&given, 8), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 8) }), "what is beside it is planned");
}
