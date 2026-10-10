use konedrive_reason::Reason;

use super::*;
use crate::model::{ContentPresence, DriveId, ItemKind, LocalItemId, OneDriveItemReference, Placement};

const ROOT: LocalItemId = LocalItemId::ROOT;

fn place(name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: ROOT, name: name.into() }
}

fn content(change_count: u64) -> ContentVersion {
    ContentVersion::ChangedHere { size: 10, mtime: 200, mtime_nsec: 7, change_count }
}

fn version(name: &str, change_count: u64) -> ItemVersion {
    ItemVersion { place: Some(place(name)), content: Some(content(change_count)) }
}

/// The row of a file with this local version, bound when `is_bound`, with a place agreed on
/// when `is_synced`.
fn row(local: Option<ItemVersion>, is_bound: bool, is_synced: bool) -> Item {
    Item {
        local_item_id: LocalItemId::new(7),
        onedrive_item: is_bound.then(|| OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new("A") }),
        kind: ItemKind::File,
        local,
        onedrive: None,
        onedrive_etag: None,
        onedrive_mime: None,
        synced_place: is_synced.then(|| place("report")),
        synced_content: None,
        content_presence: ContentPresence::NotOnDisk,
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
    }
}

fn shown(name: &str, change_count: u64) -> Item {
    row(Some(version(name, change_count)), false, false)
}

use FactStanding::{StillSaid, VersionGone};

/// An upload session is the item's while the file's local content is the content being
/// sent: not after the content changed, not once the folder does not show the file; a
/// rename leaves it.
#[test]
fn an_upload_session_stands_while_the_files_content_is_the_one_being_sent() {
    let session = UploadSession { address: UploadSessionAddress::new("https://up/1"), expires_at: None, next_offset: 0, content: content(1), placeholder: Some(place("report")) };
    assert_eq!(session.decide_standing(&shown("report", 1)), StillSaid, "the content being sent");
    assert_eq!(session.decide_standing(&shown("renamed", 1)), StillSaid, "a rename leaves the content");
    assert_eq!(session.decide_standing(&shown("report", 2)), VersionGone, "the content changed");
    assert_eq!(session.decide_standing(&row(None, true, true)), VersionGone, "the folder does not show the file");
}

/// A file too big for the space is so while its local content is the refused one.
#[test]
fn a_file_too_big_for_the_space_stands_while_its_content_is_the_refused_one() {
    let space_refusal = SpaceRefusal { content: content(1), free_space: 5 };
    assert_eq!(space_refusal.decide_standing(&shown("report", 1)), StillSaid);
    assert_eq!(space_refusal.decide_standing(&shown("renamed", 1)), StillSaid, "a rename leaves the content");
    assert_eq!(space_refusal.decide_standing(&shown("report", 2)), VersionGone, "the content changed");
    assert_eq!(space_refusal.decide_standing(&row(None, true, true)), VersionGone, "the folder does not show the file");
}

/// A file refused for lack of space fits when its size is no more than the free space.
#[test]
fn a_file_fits_when_its_size_is_no_more_than_the_free_space() {
    let space_refusal = SpaceRefusal { content: content(1), free_space: 5 };
    assert_eq!(space_refusal.decide_fit(9), SpaceFit::StillTooBig);
    assert_eq!(space_refusal.decide_fit(10), SpaceFit::Fits);
    assert_eq!(space_refusal.decide_fit(11), SpaceFit::Fits);
}

/// A refusal is the item's while its local version is the refused one, place and content;
/// a refused removal while the folder does not show the item.
#[test]
fn a_refusal_stands_while_the_local_version_is_the_refused_one() {
    let refusal = Refusal { reason: Reason::Forbidden, version: Some(version("report", 1)) };
    assert_eq!(refusal.decide_standing(&shown("report", 1)), StillSaid);
    assert_eq!(refusal.decide_standing(&shown("renamed", 1)), VersionGone, "the place changed");
    assert_eq!(refusal.decide_standing(&shown("report", 2)), VersionGone, "the content changed");
    assert_eq!(refusal.decide_standing(&row(None, true, true)), VersionGone, "removed here since");
    let refused_removal = Refusal { reason: Reason::Forbidden, version: None };
    assert_eq!(refused_removal.decide_standing(&row(None, true, true)), StillSaid, "still removed here");
    assert_eq!(refused_removal.decide_standing(&shown("report", 1)), VersionGone, "the folder shows it again");
}

/// A temporary name and a taken name are the item's while its local place is the place the
/// fact holds; a change of the content leaves both.
#[test]
fn a_temporary_name_and_a_taken_name_stand_while_the_local_place_is_the_facts() {
    let temporary = TemporaryName { name: ".konedrive-swap-A".into(), toward: place("report") };
    let taken = TakenName { place: place("report"), holder_name: "REPORT".into() };
    assert_eq!((temporary.decide_standing(&shown("report", 1)), taken.decide_standing(&shown("report", 1))), (StillSaid, StillSaid));
    assert_eq!((temporary.decide_standing(&shown("report", 2)), taken.decide_standing(&shown("report", 2))), (StillSaid, StillSaid), "the content changed");
    assert_eq!((temporary.decide_standing(&shown("renamed", 1)), taken.decide_standing(&shown("renamed", 1))), (VersionGone, VersionGone), "the place changed");
    assert_eq!((temporary.decide_standing(&row(None, true, true)), taken.decide_standing(&row(None, true, true))), (VersionGone, VersionGone), "the folder does not show it");
}

/// The confirmation of a removal is the item's while its row is a removal that is not sent:
/// bound, not shown, with a place agreed on.
#[test]
fn the_confirmation_of_a_removal_stands_while_the_row_is_a_removal_not_sent() {
    assert_eq!(decide_removal_confirmation_standing(&row(None, true, true)), StillSaid, "removed here, OneDrive not told");
    assert_eq!(decide_removal_confirmation_standing(&row(Some(version("report", 1)), true, true)), VersionGone, "the folder shows it again");
    assert_eq!(decide_removal_confirmation_standing(&row(None, true, false)), VersionGone, "the removal was confirmed by OneDrive: nothing agreed on is left");
    assert_eq!(decide_removal_confirmation_standing(&row(None, false, false)), VersionGone, "an item OneDrive does not have");
}

/// The memory is parted by what the row says: each fact with a version goes to its side,
/// and the failed tries and the item an upload left, which hold none, are always said.
#[test]
fn a_memory_is_parted_into_what_the_row_says_and_what_is_gone() {
    let memory = ItemMemory {
        upload_session: Some(UploadSession { address: UploadSessionAddress::new("https://up/1"), expires_at: Some(9), next_offset: 3, content: content(1), placeholder: None }),
        mismatched_item: Some(MismatchedItem::of_answer(OneDriveItemId::new("B"), Some("c-B"), Some("e-B"))),
        temporary_name: Some(TemporaryName { name: ".konedrive-swap-A".into(), toward: place("report") }),
        failed_tries: Some(FailedTries { count: 2, next_try_at: Some(50), last_failure: Reason::Network }),
        space_refusal: Some(SpaceRefusal { content: content(1), free_space: 5 }),
        is_removal_confirmed: true,
        refusal: Some(Refusal { reason: Reason::Forbidden, version: Some(version("report", 1)) }),
        taken_name: Some(TakenName { place: place("report"), holder_name: "REPORT".into() }),
    };
    let (said, gone) = memory.clone().split_by_standing(&shown("report", 1));
    assert_eq!(said, ItemMemory { is_removal_confirmed: false, ..memory.clone() }, "the folder shows it: all but the confirmation");
    assert_eq!(gone, ItemMemory { is_removal_confirmed: true, ..ItemMemory::default() });

    let (said, gone) = memory.clone().split_by_standing(&shown("renamed", 2));
    assert_eq!(said, ItemMemory { mismatched_item: memory.mismatched_item.clone(), failed_tries: memory.failed_tries.clone(), ..ItemMemory::default() }, "what holds no version");
    assert_eq!(gone, ItemMemory { mismatched_item: None, failed_tries: None, ..memory.clone() });

    let (said, _) = memory.clone().split_by_standing(&row(None, true, true));
    assert!(said.is_removal_confirmed && said.upload_session.is_none() && said.refusal.is_none(), "removed here: the confirmation, and nothing of the content");
}

/// What tells an item an upload left from what another client wrote since: the cTag of the
/// answer, its eTag when it carried no cTag, and nothing when it carried neither.
#[test]
fn an_item_an_upload_left_is_told_by_the_tag_of_the_answer() {
    let by_ctag = MismatchedItem::of_answer(OneDriveItemId::new("B"), Some("c1"), Some("e1"));
    assert_eq!(by_ctag.tag, UploadTag::CTag("c1".to_owned()));
    assert!(by_ctag.is_still_in_onedrive(Some("c1"), Some("e2")), "the eTag moved, the content did not");
    assert!(!by_ctag.is_still_in_onedrive(Some("c2"), Some("e1")));
    let by_etag = MismatchedItem::of_answer(OneDriveItemId::new("B"), None, Some("e1"));
    assert!(by_etag.is_still_in_onedrive(None, Some("e1")) && !by_etag.is_still_in_onedrive(Some("c1"), Some("e2")));
    assert!(MismatchedItem::of_answer(OneDriveItemId::new("B"), None, None).is_still_in_onedrive(Some("c9"), Some("e9")), "nothing says it is not");
}

/// The address of an upload session is not printed.
#[test]
fn the_address_of_an_upload_session_is_not_printed() {
    assert_eq!(format!("{:?}", UploadSessionAddress::new("https://up/secret")), "UploadSessionAddress(…)");
}
