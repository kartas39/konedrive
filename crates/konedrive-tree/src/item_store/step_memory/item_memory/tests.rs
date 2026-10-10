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

/// The row of a file of OneDrive removed here, its removal unsent since `unsent_since`.
fn removed(unsent_since: u64) -> Item {
    Item { unsent_since: Some(unsent_since), ..row(None, true, true) }
}

use FactStanding::{StillSaid, VersionGone};

/// An upload session is the item's while the bytes being sent are the file's: not after the
/// content changed, not once the folder does not show the file; a rename leaves it, and so
/// does a writer that has the file open and has not closed it yet.
#[test]
fn an_upload_session_stands_while_the_files_content_is_the_one_being_sent() {
    let session = UploadSession { address: UploadSessionAddress::new("https://up/1"), expires_at: None, next_offset: 0, content: content(1), empty_file_place: Some(place("report")) };
    assert_eq!(session.decide_standing(&shown("report", 1)), StillSaid, "the content being sent");
    assert_eq!(session.decide_standing(&shown("renamed", 1)), StillSaid, "a rename leaves the content");
    assert_eq!(session.decide_standing(&shown("report", 2)), VersionGone, "the content changed");
    assert_eq!(session.decide_standing(&row(None, true, true)), VersionGone, "the folder does not show the file");
    let opened = Item { content_before_write: Some(content(1)), ..shown("report", 2) };
    assert_eq!(session.decide_standing(&opened), StillSaid, "a writer has it open: what it was before is what is sent");
    let opened_after_another_content = Item { content_before_write: Some(content(5)), ..shown("report", 6) };
    assert_eq!(session.decide_standing(&opened_after_another_content), VersionGone);
}

/// A file too big for the space is so while the bytes that were refused are the file's.
#[test]
fn a_file_too_big_for_the_space_stands_while_its_content_is_the_refused_one() {
    let space_refusal = SpaceRefusal { content: content(1), free_space: 5 };
    assert_eq!(space_refusal.decide_standing(&shown("report", 1)), StillSaid);
    assert_eq!(space_refusal.decide_standing(&shown("renamed", 1)), StillSaid, "a rename leaves the content");
    assert_eq!(space_refusal.decide_standing(&shown("report", 2)), VersionGone, "the content changed");
    assert_eq!(space_refusal.decide_standing(&row(None, true, true)), VersionGone, "the folder does not show the file");
    assert_eq!(space_refusal.decide_standing(&Item { content_before_write: Some(content(1)), ..shown("report", 2) }), StillSaid, "a writer has it open");
}

/// A file refused for lack of space fits when its size is no more than the free space.
#[test]
fn a_file_fits_when_its_size_is_no_more_than_the_free_space() {
    let space_refusal = SpaceRefusal { content: content(1), free_space: 5 };
    assert_eq!(space_refusal.decide_fit(9), SpaceFit::StillTooBig);
    assert_eq!(space_refusal.decide_fit(10), SpaceFit::Fits);
    assert_eq!(space_refusal.decide_fit(11), SpaceFit::Fits);
}

/// A refusal of a version is the item's while it is at the refused place with the refused
/// bytes.
#[test]
fn a_refusal_of_a_version_stands_while_the_local_version_is_the_refused_one() {
    let refusal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(version("report", 1)) };
    assert_eq!(refusal.decide_standing(&shown("report", 1)), StillSaid);
    assert_eq!(refusal.decide_standing(&shown("renamed", 1)), VersionGone, "the place changed");
    assert_eq!(refusal.decide_standing(&shown("report", 2)), VersionGone, "the content changed");
    assert_eq!(refusal.decide_standing(&removed(4)), VersionGone, "removed here since");
    assert_eq!(refusal.decide_standing(&Item { content_before_write: Some(content(1)), ..shown("report", 2) }), StillSaid, "a writer has it open");
    let of_a_directory = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(ItemVersion { place: Some(place("docs")), content: None }) };
    assert_eq!(of_a_directory.decide_standing(&row(Some(ItemVersion { place: Some(place("docs")), content: None }), false, false)), StillSaid);
    assert_eq!(of_a_directory.decide_standing(&shown("docs", 1)), VersionGone, "a version with a content is not the directory's");
}

/// A confirmation of a removal and a refusal of a removal are of that removal: the row is a
/// removal that is not sent — bound, not shown, with a place agreed on — and has the stamp
/// the fact holds. A row that is a removal again with another stamp is another removal.
#[test]
fn a_fact_of_a_removal_stands_while_the_row_is_that_removal() {
    let confirmation = RemovalConfirmation { unsent_since: 4 };
    let refusal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal { unsent_since: 4 } };
    let both = |item: &Item| (confirmation.decide_standing(item), refusal.decide_standing(item));
    assert_eq!(both(&removed(4)), (StillSaid, StillSaid), "removed here, OneDrive not told");
    assert_eq!(both(&removed(9)), (VersionGone, VersionGone), "shown again and removed again: another removal");
    assert_eq!(both(&Item { unsent_since: Some(4), ..row(Some(version("report", 1)), true, true) }), (VersionGone, VersionGone), "the folder shows it");
    assert_eq!(both(&Item { unsent_since: None, ..row(None, true, false) }), (VersionGone, VersionGone), "nothing is agreed on: no removal is unsent");
    assert_eq!(both(&Item { unsent_since: Some(4), ..row(None, false, false) }), (VersionGone, VersionGone), "an item OneDrive does not have");
}

/// The removal a row is now: told by the row's stamp, and none for a row that is no removal
/// that is not sent.
#[test]
fn an_unsent_removal_is_told_by_the_stamp_of_its_row() {
    assert_eq!(UnsentRemoval::of(&removed(4)), Some(UnsentRemoval { item: LocalItemId::new(7), unsent_since: 4 }));
    assert_eq!(UnsentRemoval::of(&Item { unsent_since: Some(4), ..shown("report", 1) }), None, "the folder shows it");
    assert_eq!(UnsentRemoval::of(&row(None, true, false)), None, "the removal was sent");
}

/// A taken name is the item's while its local place is the place that was sent; a change of
/// the content leaves it.
#[test]
fn a_taken_name_stands_while_the_local_place_is_the_one_that_was_sent() {
    let taken = TakenName { place: place("report"), holder_name: "REPORT".into() };
    assert_eq!(taken.decide_standing(&shown("report", 1)), StillSaid);
    assert_eq!(taken.decide_standing(&shown("report", 2)), StillSaid, "the content changed");
    assert_eq!(taken.decide_standing(&shown("renamed", 1)), VersionGone, "the place changed");
    assert_eq!(taken.decide_standing(&row(None, true, true)), VersionGone, "the folder does not show it");
}

fn everything() -> ItemMemory {
    ItemMemory {
        upload_session: Some(UploadSession { address: UploadSessionAddress::new("https://up/1"), expires_at: Some(9), next_offset: 3, content: content(1), empty_file_place: Some(place("report")) }),
        mismatched_item: Some(MismatchedItem::of_answer(OneDriveItemId::new("B"), Some("c-B"), Some("e-B"))),
        temporary_name: Some(TemporaryName { name: ".konedrive-swap-A".into(), toward: place("report") }),
        failed_tries: Some(FailedTries { count: 2, next_try_at: Some(50), last_failure: Reason::Network }),
        space_refusal: Some(SpaceRefusal { content: content(1), free_space: 5 }),
        removal_confirmation: Some(RemovalConfirmation { unsent_since: 4 }),
        refusal: Some(Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(version("report", 1)) }),
        taken_name: Some(TakenName { place: place("report"), holder_name: "REPORT".into() }),
    }
}

/// The memory is parted by what the row says: each fact with a version goes to its side,
/// and the temporary name, the failed tries and the item an upload left, which hold none,
/// are always said.
#[test]
fn a_memory_is_parted_into_what_the_row_says_and_what_is_gone() {
    let memory = everything();
    let parts = memory.clone().split_by_standing(&shown("report", 1));
    assert_eq!(parts.said, ItemMemory { removal_confirmation: None, ..memory.clone() }, "the folder shows it: all but the confirmation");
    assert_eq!(parts.gone, ItemMemory { removal_confirmation: memory.removal_confirmation, ..ItemMemory::default() });

    let parts = memory.clone().split_by_standing(&shown("renamed", 2));
    let with_no_version = ItemMemory { mismatched_item: memory.mismatched_item.clone(), temporary_name: memory.temporary_name.clone(), failed_tries: memory.failed_tries.clone(), ..ItemMemory::default() };
    assert_eq!(parts.said, with_no_version, "what holds no version");
    assert_eq!(parts.gone, ItemMemory { mismatched_item: None, temporary_name: None, failed_tries: None, ..memory.clone() });

    let parts = memory.clone().split_by_standing(&removed(4));
    assert_eq!(parts.said, ItemMemory { removal_confirmation: memory.removal_confirmation, ..with_no_version }, "removed here: the confirmation, and nothing of the content");
}

/// A step computed the hash of a content: the facts that hold that content hold the hashed
/// one, the same bytes, and a fact of another content is left.
#[test]
fn a_content_told_by_its_hash_is_carried_into_the_facts_that_hold_it() {
    let hashed = ContentVersion::Hashed { size: 10, mtime: 200, mtime_nsec: 7, hash: Some("h".to_owned()), ctag: None };
    let mut memory = everything();
    memory.carry_content(&content(1), &hashed);
    assert_eq!(memory.upload_session.as_ref().map(|session| &session.content), Some(&hashed));
    assert_eq!(memory.space_refusal.as_ref().map(|space_refusal| &space_refusal.content), Some(&hashed));
    assert_eq!(memory.refusal.as_ref().map(|refusal| &refusal.version), Some(&RefusedVersion::Shown(ItemVersion { place: Some(place("report")), content: Some(hashed.clone()) })));

    let mut of_another_content = everything();
    of_another_content.carry_content(&content(2), &hashed);
    assert_eq!(of_another_content, everything());
}

/// A request OneDrive confirmed, a case each: the failed tries are forgotten with every
/// one; the session that sent the content of an upload ended, and one of another content is
/// left; the temporary name is forgotten by the move that reached the place it led to.
#[test]
fn what_was_kept_for_a_request_is_forgotten_once_onedrive_confirmed_it() {
    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Upload { sent: Some(&content(1)) });
    assert_eq!(memory, ItemMemory { upload_session: None, failed_tries: None, ..everything() });

    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Upload { sent: Some(&content(2)) });
    assert_eq!(memory, ItemMemory { failed_tries: None, ..everything() }, "the session sends another content: it did not end");
    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Upload { sent: None });
    assert_eq!(memory, ItemMemory { failed_tries: None, ..everything() }, "a directory was made");

    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Move { to: &place("report") });
    assert_eq!(memory, ItemMemory { temporary_name: None, failed_tries: None, ..everything() });
    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Move { to: &place(".konedrive-swap-A") });
    assert_eq!(memory, ItemMemory { failed_tries: None, ..everything() }, "the move to the temporary name itself");

    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Removal);
    assert_eq!(memory, ItemMemory { failed_tries: None, ..everything() });
}

/// An item whose binding is another: what OneDrive answered its requests is dropped, and
/// the session is given back to be cancelled; what is of its content and its place stays.
#[test]
fn an_item_with_another_binding_drops_what_onedrive_answered_its_requests() {
    let mut memory = everything();
    assert_eq!(memory.drop_requests_of_binding(), everything().upload_session);
    let kept = everything();
    assert_eq!(memory, ItemMemory { mismatched_item: kept.mismatched_item, space_refusal: kept.space_refusal, taken_name: kept.taken_name, ..ItemMemory::default() });
}

/// A directory that gave its binding up: of an item in it, the refusal of a version there,
/// the taken name there and the session whose empty file is there are dropped; the same
/// facts of another directory, and a refusal of a removal, are left.
#[test]
fn what_onedrive_answered_about_a_directory_is_dropped_when_it_gives_its_binding_up() {
    let mut memory = everything();
    assert_eq!(memory.drop_answers_of_directory(ROOT), everything().upload_session);
    assert_eq!(memory, ItemMemory { upload_session: None, refusal: None, taken_name: None, ..everything() });

    let mut of_another_directory = everything();
    assert_eq!(of_another_directory.drop_answers_of_directory(LocalItemId::new(3)), None);
    assert_eq!(of_another_directory, everything());

    let refused_removal = Some(Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal { unsent_since: 4 } });
    let mut removed_there = ItemMemory { refusal: refused_removal.clone(), ..ItemMemory::default() };
    assert_eq!(removed_there.drop_answers_of_directory(ROOT), None);
    assert_eq!(removed_there.refusal, refused_removal, "removed from the same directory of OneDrive");
}

/// What tells an item an upload left from what another client wrote since: the cTag of the
/// answer, its eTag when it carried no cTag, and nothing when it carried neither.
#[test]
fn an_item_an_upload_left_is_told_by_the_tag_of_the_answer() {
    let by_ctag = MismatchedItem::of_answer(OneDriveItemId::new("B"), Some("c1"), Some("e1"));
    assert_eq!(by_ctag.tag, UploadTag::CTag("c1".to_owned()));
    assert!(by_ctag.is_unchanged_since_upload(Some("c1"), Some("e2")), "the eTag moved, the content did not");
    assert!(!by_ctag.is_unchanged_since_upload(Some("c2"), Some("e1")));
    let by_etag = MismatchedItem::of_answer(OneDriveItemId::new("B"), None, Some("e1"));
    assert!(by_etag.is_unchanged_since_upload(None, Some("e1")) && !by_etag.is_unchanged_since_upload(Some("c1"), Some("e2")));
    assert!(MismatchedItem::of_answer(OneDriveItemId::new("B"), None, None).is_unchanged_since_upload(Some("c9"), Some("e9")), "nothing says it is not");
}

/// The address of an upload session is not printed.
#[test]
fn the_address_of_an_upload_session_is_not_printed() {
    assert_eq!(format!("{:?}", UploadSessionAddress::new("https://up/secret")), "UploadSessionAddress(…)");
}
