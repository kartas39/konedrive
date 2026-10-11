use konedrive_reason::Reason;

use super::*;
use crate::model::{ContentPresence, DriveId, ItemKind, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement};

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

/// The stamp every fact of an unsent change in [`everything`] was learnt under.
const LEARNT_UNDER: u64 = 4;

/// The row of a file the folder shows under `name` with the content `change_count`, which
/// OneDrive does not have, unsent since [`LEARNT_UNDER`].
fn shown(name: &str, change_count: u64) -> Item {
    Item { unsent_since: Some(LEARNT_UNDER), ..row(Some(version(name, change_count)), false, false) }
}

/// The row of a file of OneDrive removed here, its removal unsent since `unsent_since`.
fn removed(unsent_since: u64) -> Item {
    Item { unsent_since: Some(unsent_since), ..row(None, true, true) }
}

fn reference(item_id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(item_id) }
}

/// Every fact of a file shown as `report` with the content 1, learnt under [`LEARNT_UNDER`].
fn everything() -> ItemMemory {
    ItemMemory {
        unsent_since: Some(LEARNT_UNDER),
        upload_session: Some(UploadSession { address: UploadSessionAddress::new("https://up/1"), expires_at: Some(9), next_offset: 3, content: content(1), empty_file_place: Some(place("report")) }),
        mismatched_item: Some(MismatchedItem::of_answer(reference("B"), Some("c-B"), Some("e-B"))),
        temporary_name: Some(TemporaryName { name: ".konedrive-swap-A".into(), toward: place("report") }),
        failed_tries: Some(FailedTries { count: 2, next_try_at: Some(50), last_failure: Reason::Network }),
        space_refusal: Some(SpaceRefusal { content: content(1), free_space: 5 }),
        refusal: Some(Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(version("report", 1)) }),
        taken_name: Some(TakenName { place: place("report"), holder_name: "REPORT".into() }),
    }
}

/// What is of OneDrive in [`everything`]: under no stamp, said of any row.
fn of_onedrive() -> ItemMemory {
    ItemMemory { mismatched_item: everything().mismatched_item, temporary_name: everything().temporary_name, ..ItemMemory::default() }
}

/// What a row says of `memory`.
fn said(memory: ItemMemory, item: &Item) -> ItemMemory {
    memory.split_by_standing(item).said
}

/// The rule: a fact of an unsent change is answered only while the row has the stamp it was
/// learnt under. A row with another stamp, and a row with nothing unsent, answer none of
/// them, whatever else the row says; what is of OneDrive is under no stamp.
#[test]
fn a_fact_of_an_unsent_change_is_said_only_by_a_row_with_the_stamp_it_was_learnt_under() {
    let parts = everything().split_by_standing(&shown("report", 1));
    assert_eq!((parts.said, parts.gone), (everything(), ItemMemory::default()), "the row it was learnt of");

    let with_another_stamp = Item { unsent_since: Some(LEARNT_UNDER + 1), ..shown("report", 1) };
    let parts = everything().split_by_standing(&with_another_stamp);
    assert_eq!(parts.said, of_onedrive(), "the same versions, unsent a second time");
    assert_eq!(parts.gone, ItemMemory { mismatched_item: None, temporary_name: None, ..everything() }, "all of the first change is gone, with its stamp");

    let with_nothing_unsent = Item { unsent_since: None, ..shown("report", 1) };
    assert_eq!(said(everything(), &with_nothing_unsent), of_onedrive(), "the two sides agree: nothing is answered of what was unsent");

    let learnt_under_no_stamp = ItemMemory { unsent_since: None, ..everything() };
    assert_eq!(said(learnt_under_no_stamp, &with_nothing_unsent), of_onedrive(), "no stamp is not a stamp two can share");
}

/// Beside the stamp, an upload session and a refusal for lack of space are asked that the
/// bytes are the file's: not after the content changed, not once the folder does not show
/// the file; a rename leaves them, and so does a writer that has the file open and has not
/// closed it yet.
#[test]
fn a_session_and_a_refusal_for_lack_of_space_are_said_while_the_bytes_are_the_files() {
    let of_content = |item: &Item| {
        let said = said(everything(), item);
        (said.upload_session.is_some(), said.space_refusal.is_some())
    };
    assert_eq!(of_content(&shown("report", 1)), (true, true), "the content being sent");
    assert_eq!(of_content(&shown("renamed", 1)), (true, true), "a rename leaves the content");
    assert_eq!(of_content(&shown("report", 2)), (false, false), "the content changed again under the same stamp");
    assert_eq!(of_content(&removed(LEARNT_UNDER)), (false, false), "the folder does not show the file");
    let opened = Item { content_before_write: Some(content(1)), ..shown("report", 2) };
    assert_eq!(of_content(&opened), (true, true), "a writer has it open: what it was before is what is sent");
    let opened_after_another_content = Item { content_before_write: Some(content(5)), ..shown("report", 6) };
    assert_eq!(of_content(&opened_after_another_content), (false, false));
}

/// A file refused for lack of space fits when its size is no more than the free space.
#[test]
fn a_file_fits_when_its_size_is_no_more_than_the_free_space() {
    let space_refusal = SpaceRefusal { content: content(1), free_space: 5 };
    assert_eq!(space_refusal.decide_fit(9), SpaceFit::StillTooBig);
    assert_eq!(space_refusal.decide_fit(10), SpaceFit::Fits);
    assert_eq!(space_refusal.decide_fit(11), SpaceFit::Fits);
}

/// Beside the stamp, a refusal of a shown version is asked that the item is at the refused
/// place with the refused bytes, and a refusal of a removal that the row is a removal.
#[test]
fn a_refusal_is_said_while_the_row_is_the_version_or_the_removal_that_was_refused() {
    let is_said = |refusal: &Refusal, item: &Item| said(ItemMemory { unsent_since: Some(LEARNT_UNDER), refusal: Some(refusal.clone()), ..ItemMemory::default() }, item).refusal.is_some();
    let refusal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(version("report", 1)) };
    assert!(is_said(&refusal, &shown("report", 1)));
    assert!(!is_said(&refusal, &shown("renamed", 1)), "the place changed");
    assert!(!is_said(&refusal, &shown("report", 2)), "the content changed");
    assert!(!is_said(&refusal, &removed(LEARNT_UNDER)), "removed here since, unsent all the while");
    assert!(is_said(&refusal, &Item { content_before_write: Some(content(1)), ..shown("report", 2) }), "a writer has it open");
    let of_a_directory = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(ItemVersion { place: Some(place("docs")), content: None }) };
    assert!(is_said(&of_a_directory, &Item { unsent_since: Some(LEARNT_UNDER), ..row(Some(ItemVersion { place: Some(place("docs")), content: None }), false, false) }));
    assert!(!is_said(&of_a_directory, &shown("docs", 1)), "a version with a content is not the directory's");

    let of_a_removal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal };
    assert!(is_said(&of_a_removal, &removed(LEARNT_UNDER)), "removed here, OneDrive not told");
    assert!(!is_said(&of_a_removal, &removed(9)), "shown again and removed again: another removal");
    assert!(!is_said(&of_a_removal, &Item { unsent_since: Some(LEARNT_UNDER), ..row(Some(version("report", 1)), true, true) }), "the folder shows it");
}

/// Beside the stamp, a taken name is asked that the item's local place is the place that
/// was sent; a change of the content leaves it. The failed tries are asked nothing more.
#[test]
fn a_taken_name_is_said_while_the_local_place_is_the_one_that_was_sent() {
    let of = |item: &Item| {
        let said = said(everything(), item);
        (said.taken_name.is_some(), said.failed_tries.is_some())
    };
    assert_eq!(of(&shown("report", 1)), (true, true));
    assert_eq!(of(&shown("report", 2)), (true, true), "the content changed");
    assert_eq!(of(&shown("renamed", 1)), (false, true), "the place changed");
    assert_eq!(of(&removed(LEARNT_UNDER)), (false, true), "the folder does not show it, and it is unsent still");
}

/// The memory holds the stamp only while it holds a fact that was learnt under it.
#[test]
fn a_memory_with_no_fact_of_an_unsent_change_holds_no_stamp() {
    let parts = everything().split_by_standing(&shown("renamed", 2));
    assert_eq!(parts.said, ItemMemory { unsent_since: Some(LEARNT_UNDER), failed_tries: everything().failed_tries, ..of_onedrive() }, "the tries are of the stamp alone");
    let mut memory = ItemMemory { unsent_since: Some(LEARNT_UNDER), ..of_onedrive() };
    memory.keep_stamp_only_with_its_facts();
    assert_eq!(memory, of_onedrive());
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

/// A request OneDrive confirmed of an item it has, a case each. An upload and a move: the
/// failed tries are forgotten, whatever the stamp. A move: the taken name of the place it
/// was confirmed to, and the temporary name unless the move was to that very name. The upload session is
/// not forgotten here: the one that ended is told by its address.
#[test]
fn what_a_confirmed_request_ends_is_forgotten_whatever_the_stamp() {
    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Upload { ended_session: None });
    assert_eq!(memory, ItemMemory { failed_tries: None, ..everything() });

    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Move { to: &place("report") });
    assert_eq!(memory, ItemMemory { temporary_name: None, failed_tries: None, taken_name: None, ..everything() }, "the place the name led to, which was answered as taken: OneDrive has the item there");
    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Move { to: &place("elsewhere") });
    assert_eq!(memory, ItemMemory { temporary_name: None, failed_tries: None, ..everything() }, "any other place: OneDrive does not have it under the temporary name");
    let mut memory = everything();
    memory.forget_confirmed_request(ConfirmedRequest::Move { to: &place(".konedrive-swap-A") });
    assert_eq!(memory, ItemMemory { failed_tries: None, ..everything() }, "the move to the temporary name itself");

    let mut sent_elsewhere = ItemMemory { taken_name: Some(TakenName { place: place("taken"), holder_name: "TAKEN".into() }), ..everything() };
    let kept_taken_name = sent_elsewhere.taken_name.clone();
    sent_elsewhere.forget_confirmed_request(ConfirmedRequest::Move { to: &place("report") });
    assert_eq!(sent_elsewhere.taken_name, kept_taken_name, "a name taken at another place than OneDrive confirmed");

    let mut only_tries = ItemMemory { unsent_since: Some(LEARNT_UNDER), failed_tries: everything().failed_tries, ..ItemMemory::default() };
    only_tries.forget_confirmed_request(ConfirmedRequest::Upload { ended_session: None });
    assert_eq!(only_tries, ItemMemory::default(), "and the stamp with its last fact");
}

/// An operation on the item's binding: every fact of an unsent change is dropped, with the
/// stamp; the session is given back to be cancelled and the temporary name to go with the
/// binding; the item an upload left stays.
#[test]
fn an_operation_on_the_items_binding_drops_every_fact_of_an_unsent_change() {
    let mut memory = everything();
    assert_eq!(memory.drop_facts_of_binding(), (everything().upload_session, everything().temporary_name));
    assert_eq!(memory, ItemMemory { mismatched_item: everything().mismatched_item, ..ItemMemory::default() });
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

    let refused_removal = Some(Refusal { reason: Reason::Forbidden, version: RefusedVersion::Removal });
    let mut removed_there = ItemMemory { unsent_since: Some(LEARNT_UNDER), refusal: refused_removal.clone(), ..ItemMemory::default() };
    assert_eq!(removed_there.drop_answers_of_directory(ROOT), None);
    assert_eq!((removed_there.refusal, removed_there.unsent_since), (refused_removal, Some(LEARNT_UNDER)), "removed from the same directory of OneDrive");
}

/// What tells an item an upload left from what another client wrote since: the cTag of the
/// answer, its eTag when it carried no cTag, and nothing when it carried neither.
#[test]
fn an_item_an_upload_left_is_told_by_the_tag_of_the_answer() {
    let by_ctag = MismatchedItem::of_answer(reference("B"), Some("c1"), Some("e1"));
    assert_eq!(by_ctag.tag, UploadTag::CTag("c1".to_owned()));
    assert!(by_ctag.is_unchanged_since_upload(Some("c1"), Some("e2")), "the eTag moved, the content did not");
    assert!(!by_ctag.is_unchanged_since_upload(Some("c2"), Some("e1")));
    let by_etag = MismatchedItem::of_answer(reference("B"), None, Some("e1"));
    assert!(by_etag.is_unchanged_since_upload(None, Some("e1")) && !by_etag.is_unchanged_since_upload(Some("c1"), Some("e2")));
    assert!(MismatchedItem::of_answer(reference("B"), None, None).is_unchanged_since_upload(Some("c9"), Some("e9")), "nothing says it is not");
}

/// The address of an upload session is not printed.
#[test]
fn the_address_of_an_upload_session_is_not_printed() {
    assert_eq!(format!("{:?}", UploadSessionAddress::new("https://up/secret")), "UploadSessionAddress(…)");
}

/// The failed tries are of the unsent change the memory's stamp tells, a case each: said
/// while the row has that stamp, whatever OneDrive has of the item, and not for another
/// stamp or for a row nothing is unsent of.
#[test]
fn the_failed_tries_are_said_while_the_row_has_the_stamp_they_were_learnt_under() {
    let with_etag = |etag: &str, unsent_since: Option<u64>| Item { onedrive_etag: Some(etag.to_owned()), unsent_since, ..row(Some(version("report", 1)), true, true) };
    let tries = FailedTries { count: 1, next_try_at: Some(50), last_failure: Reason::Network };
    let memory = ItemMemory { unsent_since: Some(LEARNT_UNDER), failed_tries: Some(tries), ..ItemMemory::default() };
    let is_said = |item: &Item| said(memory.clone(), item).failed_tries.is_some();
    assert!(is_said(&with_etag("e1", Some(LEARNT_UNDER))));
    assert!(is_said(&with_etag("e2", Some(LEARNT_UNDER))), "the stamp, whatever OneDrive has");
    assert!(!is_said(&with_etag("e1", Some(LEARNT_UNDER + 1))), "another unsent change");
    assert!(!is_said(&with_etag("e1", None)), "nothing unsent");
}

/// A listing wrote where OneDrive has the item: the temporary name stays only when that is
/// the temporary name's own place.
#[test]
fn a_temporary_name_is_forgotten_once_onedrive_has_the_item_at_another_place() {
    let mut memory = everything();
    memory.forget_temporary_name_unless_at(&place(".konedrive-swap-A"));
    assert_eq!(memory, everything());
    memory.forget_temporary_name_unless_at(&place("elsewhere"));
    assert_eq!(memory, ItemMemory { temporary_name: None, ..everything() });
}
