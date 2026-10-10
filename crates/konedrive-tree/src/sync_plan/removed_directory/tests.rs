use super::*;
use crate::sync_plan::tests::{id, removed_here, synced_file, ROOT};

fn removed_with_the_directory(number: i64) -> ItemVersions {
    ItemVersions::of(&removed_here(synced_file(number, id(2), &format!("f{number}"))))
}

/// The design's "a directory removed here with everything below it removed here too is one
/// removal", and "something changed below it in OneDrive: the removal is sent and stands".
#[test]
fn a_removal_is_sent_for_all_that_was_removed_with_the_directory_and_all_onedrive_made_in_it() {
    assert_eq!(decide_directory_removal(&[]), DirectoryRemovalOutcome::IsSent { removed_with_it: Vec::new() });
    let below = [ItemBelowRemovedDirectory::RemovedHere(removed_with_the_directory(3)), ItemBelowRemovedDirectory::NeverShownHere(id(4)), ItemBelowRemovedDirectory::RemovedHere(removed_with_the_directory(5))];
    assert_eq!(decide_directory_removal(&below), DirectoryRemovalOutcome::IsSent { removed_with_it: vec![removed_with_the_directory(3), removed_with_the_directory(5)] });
}

/// The design's "a directory's removal waits for every unsent change of what OneDrive has
/// below it, so that an item moved out of it is not removed with it".
#[test]
fn a_removal_waits_for_an_item_the_folder_shows() {
    let below = [ItemBelowRemovedDirectory::RemovedHere(removed_with_the_directory(3)), ItemBelowRemovedDirectory::ShownHere(id(4)), ItemBelowRemovedDirectory::ShownHere(id(5))];
    assert_eq!(decide_directory_removal(&below), DirectoryRemovalOutcome::WaitsFor(id(4)));
}

/// The review's M1: one step per item, also when the item goes with another's removal.
#[test]
fn a_removal_waits_for_an_item_a_step_is_running_for() {
    let below = [ItemBelowRemovedDirectory::NeverShownHere(id(3)), ItemBelowRemovedDirectory::HasRunningStep(id(4))];
    assert_eq!(decide_directory_removal(&below), DirectoryRemovalOutcome::WaitsFor(id(4)));
    let _ = ROOT;
}
