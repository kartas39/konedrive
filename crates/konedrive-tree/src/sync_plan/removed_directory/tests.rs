use super::*;
use crate::sync_plan::tests::{hashed, id, listed, moved_in_onedrive, removed_here, synced_directory, synced_file, with_content_in_onedrive};

fn removed_with_the_directory(number: i64) -> ItemVersions {
    ItemVersions::of(&removed_here(synced_file(number, id(2), &format!("f{number}"))))
}

use DirectoryRemovalOutcome::{IsOfFolderAlone, IsSent, WaitsFor, WaitsForListing};
use ItemBelowRemovedDirectory::{ContentChangedInOneDrive, IsHeldBack, NeverShownHere, RemovedHere, ShownHere};

/// The design's "a directory removed here with everything below it removed here too is one
/// removal": the positive condition. An empty directory is the same.
#[test]
fn a_removal_is_sent_when_all_onedrive_has_below_the_directory_was_removed_here() {
    assert_eq!(decide_directory_removal(false, &[]), IsSent { removed_with_it: Vec::new() });
    let below = [RemovedHere(removed_with_the_directory(3)), RemovedHere(removed_with_the_directory(5))];
    assert_eq!(decide_directory_removal(false, &below), IsSent { removed_with_it: vec![removed_with_the_directory(3), removed_with_the_directory(5)] });
}

/// The brief's §8: what the folder never showed is not removed with a directory. One such
/// item anywhere below makes the removal one of the folder alone.
#[test]
fn a_removal_is_of_the_folder_alone_when_onedrive_has_below_the_directory_what_the_folder_never_showed() {
    for below in [vec![NeverShownHere(id(4))], vec![RemovedHere(removed_with_the_directory(3)), NeverShownHere(id(4)), RemovedHere(removed_with_the_directory(5))]] {
        assert_eq!(decide_directory_removal(false, &below), IsOfFolderAlone);
    }
}

/// The third review's S3: a file that was removed here and whose content OneDrive changed
/// since comes back by itself, and so it is not removed with a directory either. One such
/// file anywhere below makes the removal one of the folder alone, as what was never shown
/// does.
#[test]
fn a_removal_is_of_the_folder_alone_when_onedrive_changed_the_content_of_a_file_that_was_removed_here_below_the_directory() {
    for below in [vec![ContentChangedInOneDrive(id(4))], vec![RemovedHere(removed_with_the_directory(3)), ContentChangedInOneDrive(id(4)), RemovedHere(removed_with_the_directory(5))]] {
        assert_eq!(decide_directory_removal(false, &below), IsOfFolderAlone);
    }
    assert_eq!(decide_directory_removal(false, &[ContentChangedInOneDrive(id(3)), ShownHere(id(4))]), WaitsFor(id(4)), "what holds comes first");
    assert_eq!(decide_directory_removal(true, &[ContentChangedInOneDrive(id(3))]), WaitsForListing);
}

/// The design's "a directory's removal waits for every unsent change of what OneDrive has
/// below it, so that an item moved out of it is not removed with it"; and the review's M1:
/// one step per item, also when the item would go with another's removal. What holds comes
/// before what makes the removal one of the folder alone.
#[test]
fn a_removal_waits_for_the_first_item_the_folder_shows_or_that_is_held_back() {
    assert_eq!(decide_directory_removal(false, &[RemovedHere(removed_with_the_directory(3)), ShownHere(id(4)), ShownHere(id(5))]), WaitsFor(id(4)));
    assert_eq!(decide_directory_removal(false, &[NeverShownHere(id(3)), IsHeldBack(id(4))]), WaitsFor(id(4)));
    assert_eq!(decide_directory_removal(false, &[IsHeldBack(id(3)), ShownHere(id(4))]), WaitsFor(id(3)));
}

/// While a listing is under way nothing is decided, whatever is below.
#[test]
fn a_removal_waits_for_a_listing_under_way_whatever_is_below_the_directory() {
    for below in [vec![], vec![RemovedHere(removed_with_the_directory(3))], vec![NeverShownHere(id(3))], vec![ShownHere(id(3))], vec![IsHeldBack(id(3))]] {
        assert_eq!(decide_directory_removal(true, &below), WaitsForListing);
    }
}

/// What a row OneDrive has below a removed directory is, a case each, the first that
/// holds: shown, held back, a file removed here whose content OneDrive changed, removed
/// here (something is agreed on of it), never shown.
#[test]
fn an_item_below_a_removed_directory_is_told_by_its_row_and_by_whether_it_is_held_back() {
    let (shown, removed, never_shown) = (synced_file(3, id(2), "a"), removed_here(synced_file(3, id(2), "a")), listed(3, ItemKind::File, id(2), "a"));
    let content_alone = Item { synced_place: None, ..removed.clone() };
    let changed = with_content_in_onedrive(removed.clone(), hashed("theirs"));
    let retimed = with_content_in_onedrive(removed.clone(), crate::model::ContentVersion::Hashed { size: 3, mtime: 900, mtime_nsec: 0, hash: Some("h3".to_owned()), ctag: Some("c-h3".to_owned()) });
    let moved_into_it = moved_in_onedrive(removed_here(synced_file(3, id(1), "a")), id(2), "a");
    let removed_directory = removed_here(synced_directory(3, id(2), "e"));
    for (what, row, is_held_back, is) in [
        ("removed here, its content changed in OneDrive", &changed, false, ContentChangedInOneDrive(id(3))),
        ("removed here, its content changed in OneDrive, and held back", &changed, true, IsHeldBack(id(3))),
        ("shown, its content changed in OneDrive", &with_content_in_onedrive(shown.clone(), hashed("theirs")), false, ShownHere(id(3))),
        ("removed here, the time of its content changed in OneDrive", &retimed, false, RemovedHere(ItemVersions::of(&retimed))),
        ("removed here elsewhere, moved into the directory in OneDrive", &moved_into_it, false, RemovedHere(ItemVersions::of(&moved_into_it))),
        ("a directory removed here", &removed_directory, false, RemovedHere(ItemVersions::of(&removed_directory))),
        ("shown", &shown, false, ShownHere(id(3))),
        ("shown, and held back", &shown, true, ShownHere(id(3))),
        ("removed here", &removed, false, RemovedHere(ItemVersions::of(&removed))),
        ("removed here, and held back", &removed, true, IsHeldBack(id(3))),
        ("a content agreed on alone", &content_alone, false, RemovedHere(ItemVersions::of(&content_alone))),
        ("never shown", &never_shown, false, NeverShownHere(id(3))),
        ("never shown, and held back", &never_shown, true, IsHeldBack(id(3))),
    ] {
        assert_eq!(ItemBelowRemovedDirectory::of(row, is_held_back, false, false), is, "{what}");
    }
}
