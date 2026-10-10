use super::*;
use crate::model::ItemKind;
use crate::sync_plan::tests::{hashed_here, id, listed, made_directory, made_file, moved_here, moved_in_onedrive, synced_file, with_content_here, written_here, ROOT};

fn claim<'a>(claimant: &'a Item, holder: Option<NameHolder<'a>>) -> NameClaim<'a> {
    NameClaim { claimant, is_left_here: false, holder, has_earlier_rival: false, is_answered_taken: false, is_listing_under_way: false }
}

#[test]
fn a_name_nothing_else_has_or_wants_is_free() {
    assert_eq!(decide_name_contest(&claim(&made_file(2, ROOT, "a.txt"), None)), NameContestOutcome::NameIsFree);
}

/// The design's "what frees a name goes before what takes it", towards OneDrive.
#[test]
fn an_item_waits_for_the_holder_that_leaves_the_name() {
    let new = made_file(2, ROOT, "a.txt");
    assert_eq!(decide_name_contest(&claim(&new, Some(NameHolder::Leaves(id(3))))), NameContestOutcome::WaitsFor(id(3)));
    // An item that is never sent waits for nothing.
    assert_eq!(decide_name_contest(&NameClaim { is_left_here: true, ..claim(&new, Some(NameHolder::Leaves(id(3)))) }), NameContestOutcome::NameIsFree);
}

/// The design's "two local items, one name for OneDrive".
#[test]
fn the_later_of_two_items_of_the_folder_steps_aside() {
    let new = made_file(3, ROOT, "A.txt");
    assert_eq!(decide_name_contest(&NameClaim { has_earlier_rival: true, ..claim(&new, None) }), NameContestOutcome::StepsAside);
    assert_eq!(decide_name_contest(&NameClaim { has_earlier_rival: true, ..claim(&new, Some(NameHolder::Leaves(id(4)))) }), NameContestOutcome::StepsAside);
}

/// The design's "a new item here and a new one in OneDrive of one name".
#[test]
fn two_new_directories_and_two_new_files_of_one_hash_are_one_item() {
    let onedrive_directory = listed(3, ItemKind::Directory, ROOT, "d");
    assert_eq!(decide_name_contest(&claim(&made_directory(2, ROOT, "d"), Some(NameHolder::Stays(&onedrive_directory)))), NameContestOutcome::MergesWith(id(3)));
    // The content of `listed(3, …)` is of the hash `h3` and of size 3.
    let onedrive_file = listed(3, ItemKind::File, ROOT, "a.txt");
    let stays = Some(NameHolder::Stays(&onedrive_file));
    let same = with_content_here(made_file(2, ROOT, "a.txt"), hashed_here("h3"));
    assert_eq!(decide_name_contest(&claim(&same, stays)), NameContestOutcome::MergesWith(id(3)));
    let other = with_content_here(made_file(2, ROOT, "a.txt"), hashed_here("mine"));
    assert_eq!(decide_name_contest(&claim(&other, stays)), NameContestOutcome::StepsAside);
    let not_hashed = with_content_here(made_file(2, ROOT, "a.txt"), written_here());
    assert_eq!(decide_name_contest(&claim(&not_hashed, stays)), NameContestOutcome::HashesLocalContent);
    let longer = with_content_here(made_file(2, ROOT, "a.txt"), crate::model::ContentVersion::ChangedHere { size: 4, mtime: 200, mtime_nsec: 0, change_count: 1 });
    assert_eq!(decide_name_contest(&claim(&longer, stays)), NameContestOutcome::StepsAside, "another size is another content, with no hash");
    assert_eq!(decide_name_contest(&claim(&made_directory(2, ROOT, "a.txt"), stays)), NameContestOutcome::StepsAside, "a directory and a file");
}

#[test]
fn an_item_steps_aside_for_a_holder_that_stays_and_is_not_one_item_with_it() {
    // The holder is no new item of OneDrive: OneDrive moved it to the name.
    let moved = moved_in_onedrive(synced_file(3, ROOT, "b.txt"), ROOT, "a.txt");
    assert_eq!(decide_name_contest(&claim(&with_content_here(made_file(2, ROOT, "a.txt"), hashed_here("h3")), Some(NameHolder::Stays(&moved)))), NameContestOutcome::StepsAside);
    // The claimant is no new item of the folder: it was moved here to the name.
    let new_in_onedrive = listed(3, ItemKind::File, ROOT, "a.txt");
    let moved_here = moved_here(synced_file(2, ROOT, "b.txt"), ROOT, "a.txt");
    assert_eq!(decide_name_contest(&claim(&moved_here, Some(NameHolder::Stays(&new_in_onedrive)))), NameContestOutcome::StepsAside);
    // An item that is left here is never one with an item of OneDrive.
    let onedrive_directory = listed(3, ItemKind::Directory, ROOT, "d");
    let left_here = made_directory(2, ROOT, "d");
    assert_eq!(decide_name_contest(&NameClaim { is_left_here: true, ..claim(&left_here, Some(NameHolder::Stays(&onedrive_directory))) }), NameContestOutcome::StepsAside);
}

/// The design's "while a listing is being written": two rows may say one name on the
/// OneDrive side, and a later page may move the holder away.
#[test]
fn nothing_is_decided_against_a_holder_that_stays_while_a_listing_is_under_way() {
    let onedrive_directory = listed(3, ItemKind::Directory, ROOT, "d");
    let new = made_directory(2, ROOT, "d");
    assert_eq!(decide_name_contest(&NameClaim { is_listing_under_way: true, ..claim(&new, Some(NameHolder::Stays(&onedrive_directory))) }), NameContestOutcome::WaitsForListing);
}

/// The design's "OneDrive is the judge": the plan's rule finds nothing under the name, and
/// OneDrive answered that the name is taken.
#[test]
fn an_item_steps_aside_from_a_name_onedrive_answered_is_taken() {
    let new = made_file(2, ROOT, "a.txt");
    let answered_taken = NameClaim { is_answered_taken: true, ..claim(&new, None) };
    assert_eq!(decide_name_contest(&answered_taken), NameContestOutcome::StepsAside);
    assert_eq!(decide_name_contest(&NameClaim { is_listing_under_way: true, ..answered_taken }), NameContestOutcome::WaitsForListing, "a page to come may bring the holder");
    // A holder the rule finds decides as it does with no answer.
    assert_eq!(decide_name_contest(&NameClaim { is_answered_taken: true, ..claim(&new, Some(NameHolder::Leaves(id(3)))) }), NameContestOutcome::WaitsFor(id(3)));
    let onedrive_directory = listed(3, ItemKind::Directory, ROOT, "d");
    let new_directory = made_directory(2, ROOT, "d");
    assert_eq!(decide_name_contest(&NameClaim { is_answered_taken: true, ..claim(&new_directory, Some(NameHolder::Stays(&onedrive_directory))) }), NameContestOutcome::MergesWith(id(3)));
}
