use super::*;
use crate::model::ItemKind;
use crate::sync_plan::tests::{hashed_here, id, listed, made_directory, made_file, moved_here, moved_in_onedrive, synced_file, with_content_here, written_here, ROOT};

fn claim<'a>(claimant: &'a Item, holder: Option<NameHolder<'a>>) -> NameClaim<'a> {
    NameClaim { claimant, is_left_here: false, holder, has_earlier_rival: false, is_answered_taken: false, holder_cannot_be_shown: false, has_the_name_in_onedrive: false, is_listing_under_way: false }
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

/// The review's M2, the brief's §7: nothing of the user's is one with an item the folder
/// cannot show; OneDrive holds the name, so the local item steps aside.
#[test]
fn a_new_item_is_not_merged_with_an_item_the_folder_cannot_show() {
    let onedrive_directory = listed(3, ItemKind::Directory, ROOT, "shared");
    let new = made_directory(2, ROOT, "shared");
    let cannot_be_shown = NameClaim { holder_cannot_be_shown: true, ..claim(&new, Some(NameHolder::Stays(&onedrive_directory))) };
    assert_eq!(decide_name_contest(&cannot_be_shown), NameContestOutcome::StepsAside);
    assert_eq!(decide_name_contest(&NameClaim { is_listing_under_way: true, ..cannot_be_shown }), NameContestOutcome::WaitsForListing, "a page to come may say the folder can show it");
}

/// The class behind the review's M1: a holder a step is running for is waited for, not
/// merged with and not stepped aside from; the plan made after that step decides.
#[test]
fn an_item_waits_for_a_holder_that_is_held_back() {
    let new = made_directory(2, ROOT, "d");
    assert_eq!(decide_name_contest(&claim(&new, Some(NameHolder::IsHeldBack(id(3))))), NameContestOutcome::WaitsFor(id(3)));
    assert_eq!(decide_name_contest(&NameClaim { is_left_here: true, ..claim(&new, Some(NameHolder::IsHeldBack(id(3)))) }), NameContestOutcome::NameIsFree);
}

/// The second review's S1: OneDrive has the claimant under a name of this fold already,
/// beside the holder. It has judged: the two are kept apart, whatever the plan's rule for
/// comparing names says, once no listing is under way. An answer of OneDrive that the name
/// is taken overrules that, as it overrules the rule.
#[test]
fn a_name_onedrive_keeps_apart_from_the_holders_is_free() {
    let renamed = moved_here(synced_file(2, ROOT, "\u{e9}clair"), ROOT, "\u{c9}CLAIR");
    let beside = synced_file(3, ROOT, "e\u{301}clair");
    let within_the_fold = NameClaim { has_the_name_in_onedrive: true, ..claim(&renamed, Some(NameHolder::Stays(&beside))) };
    assert_eq!(decide_name_contest(&within_the_fold), NameContestOutcome::NameIsFree);
    assert_eq!(decide_name_contest(&NameClaim { holder: Some(NameHolder::Leaves(id(3))), ..within_the_fold }), NameContestOutcome::NameIsFree);
    assert_eq!(decide_name_contest(&NameClaim { is_listing_under_way: true, ..within_the_fold }), NameContestOutcome::WaitsForListing);
    assert_eq!(decide_name_contest(&NameClaim { is_answered_taken: true, ..within_the_fold }), NameContestOutcome::StepsAside);
    assert_eq!(decide_name_contest(&NameClaim { has_earlier_rival: true, ..within_the_fold }), NameContestOutcome::StepsAside);
    assert_eq!(decide_name_contest(&NameClaim { has_the_name_in_onedrive: false, ..within_the_fold }), NameContestOutcome::StepsAside, "it comes to the name from elsewhere");
}

/// What an upload of this folder left in OneDrive holds a name to every file but the one it
/// was left by: no file is merged with it, and none steps aside for it. The claimant waits
/// for the file whose sending removes it; where that file's row went, for the removal the
/// daemon sends by itself; and for nothing when it is left here.
#[test]
fn a_claimant_waits_for_the_removal_of_what_an_upload_left_under_its_name() {
    let new = made_file(2, ROOT, "a.txt");
    let (of_a_file, of_no_file) = (NameHolder::IsLeftByUpload { file: Some(id(5)) }, NameHolder::IsLeftByUpload { file: None });
    assert_eq!(decide_name_contest(&claim(&new, Some(of_a_file))), NameContestOutcome::WaitsFor(id(5)));
    assert_eq!(decide_name_contest(&claim(&new, Some(of_no_file))), NameContestOutcome::WaitsForRemovalOfHolder);
    for holder in [of_a_file, of_no_file] {
        assert_eq!(decide_name_contest(&NameClaim { is_left_here: true, ..claim(&new, Some(holder)) }), NameContestOutcome::NameIsFree, "{holder:?}: never sent");
        assert_eq!(decide_name_contest(&NameClaim { has_earlier_rival: true, ..claim(&new, Some(holder)) }), NameContestOutcome::StepsAside, "{holder:?}: for the rival, not for what was left");
        assert_ne!(decide_name_contest(&NameClaim { is_answered_taken: true, ..claim(&new, Some(holder)) }), NameContestOutcome::StepsAside, "{holder:?}: answered taken");
    }
    // A listing under way changes nothing: what was left is not what a page may move.
    assert_eq!(decide_name_contest(&NameClaim { is_listing_under_way: true, ..claim(&new, Some(of_no_file)) }), NameContestOutcome::WaitsForRemovalOfHolder);
}
