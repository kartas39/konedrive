use std::ffi::OsString;

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, FolderStep, ItemPlace, ItemVersion, OneDriveItemId, OneDriveItemReference, OneDriveStep, RowStep};

use super::*;

fn id(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn place(name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: OsString::from(name) }
}

fn content(hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

fn reference(number: i64) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(format!("I{number}")) }
}

/// A file of OneDrive the folder shows, the two sides agreeing on it, its content not on
/// disk: what a download of a pinned file is planned from.
fn agreeing(number: i64) -> ItemVersions {
    let version = ItemVersion { place: Some(place("report")), content: Some(content("theirs")) };
    ItemVersions {
        local_item_id: id(number),
        onedrive_item: Some(reference(number)),
        local: Some(version.clone()),
        onedrive: Some(version),
        onedrive_etag: Some("e1".to_owned()),
        synced_place: Some(place("report")),
        synced_content: Some(content("theirs")),
        content_presence: ContentPresence::NotOnDisk,
        unsent_since: None,
    }
}

fn download(item: &ItemVersions) -> LocalStep {
    LocalStep::of(&SyncStep::Folder(FolderStep::DownloadContent { item: item.clone() })).expect("towards the folder")
}

fn rows(items: &[&ItemVersions]) -> BTreeMap<LocalItemId, ItemVersions> {
    items.iter().map(|item| (item.local_item_id, (*item).clone())).collect()
}

/// The tries kept of a download of item 2 that failed twice, planned from `planned_from`.
fn tries_of_a_download(planned_from: &ItemVersions) -> LocalStepTries {
    let mut tries = LocalStepTries::default();
    assert_eq!(tries.remember_failed_try(&download(planned_from), Reason::Network, Some(60)), 1);
    assert_eq!(tries.remember_failed_try(&download(planned_from), Reason::Network, Some(120)), 2);
    tries
}

/// Whether the tries of the download are kept still once a plan's reading has `now` as the
/// row of item 2.
fn are_kept_for(now: &ItemVersions) -> bool {
    let mut tries = tries_of_a_download(&agreeing(2));
    tries.void_by_rows(&rows(&[now]));
    tries.count_tries(&download(now)) > 0 || !tries.list_items().is_empty()
}

#[test]
fn a_failed_try_is_counted_and_told_to_the_plan_with_its_next_try() {
    let planned_from = agreeing(2);
    let tries = tries_of_a_download(&planned_from);
    assert_eq!(tries.count_tries(&download(&planned_from)), 2);
    let kind = LocalStepKind::TowardsFolder(FolderStepKind::DownloadContent);
    assert_eq!(tries.find_tries(id(2), kind).map(|kept| (kept.count, kept.next_try_at, kept.last_failure.clone())), Some((2, Some(120), Reason::Network)));
    assert_eq!(tries.list_next_tries(), [((id(2), TriedStep::TowardsFolder(FolderStepKind::DownloadContent)), NextTry { not_before: 120, reason: Reason::Network })].into());
    assert_eq!(tries.list_items(), [id(2)].into());

    let mut tries = LocalStepTries::default();
    assert_eq!(tries.remember_failed_try(&download(&planned_from), Reason::Failed, None), 1);
    assert_eq!(tries.list_next_tries(), BTreeMap::new(), "a try due at once makes nothing wait");
    assert_eq!(tries.count_tries(&download(&planned_from)), 1);
}

#[test]
fn the_tries_of_a_step_end_when_it_is_written() {
    let planned_from = agreeing(2);
    let mut tries = tries_of_a_download(&planned_from);
    tries.forget_tries(&download(&planned_from));
    assert_eq!(tries, LocalStepTries::default());
    assert_eq!(tries.remember_failed_try(&download(&planned_from), Reason::Network, Some(60)), 1, "the next failure is the first");
}

#[test]
fn the_tries_end_when_the_local_version_the_step_works_from_changes() {
    assert!(are_kept_for(&agreeing(2)), "the row as the step was planned from");
    let mut renamed = agreeing(2);
    renamed.local.as_mut().unwrap().place = Some(place("renamed"));
    renamed.unsent_since = Some(7);
    assert!(!are_kept_for(&renamed));
    let mut removed = agreeing(2);
    removed.local = None;
    assert!(!are_kept_for(&removed));
}

#[test]
fn the_tries_end_when_the_onedrive_version_the_step_works_from_changes() {
    let mut changed = agreeing(2);
    changed.onedrive.as_mut().unwrap().content = Some(content("theirs-2"));
    assert!(!are_kept_for(&changed));
    let mut moved = agreeing(2);
    moved.onedrive.as_mut().unwrap().place = Some(place("elsewhere"));
    assert!(!are_kept_for(&moved));
    let mut removed = agreeing(2);
    (removed.onedrive, removed.onedrive_etag) = (None, None);
    assert!(!are_kept_for(&removed));
    let mut bound_to_another = agreeing(2);
    bound_to_another.onedrive_item = Some(reference(9));
    assert!(!are_kept_for(&bound_to_another));
}

/// The download of a pinned file fails, and a read
/// downloads the content. No step was written, and the work is done.
#[test]
fn the_tries_end_when_the_content_comes_to_be_on_disk_by_another_way() {
    let mut landed = agreeing(2);
    landed.content_presence = ContentPresence::OnDisk;
    assert!(!are_kept_for(&landed));
    let mut tries = tries_of_a_download(&agreeing(2));
    tries.void_by_rows(&rows(&[&landed]));
    assert_eq!((tries.list_next_tries(), tries.clone()), (BTreeMap::new(), LocalStepTries::default()), "nothing waits, and nothing is kept");
    // The copy is freed later and downloaded again: the first failure is the first.
    assert_eq!(tries.remember_failed_try(&download(&agreeing(2)), Reason::Network, Some(60)), 1);
}

#[test]
fn the_tries_end_when_what_the_two_sides_agreed_on_changes() {
    let mut agreed_elsewhere = agreeing(2);
    agreed_elsewhere.synced_place = Some(place("elsewhere"));
    assert!(!are_kept_for(&agreed_elsewhere));
    let mut agreed_on_another_content = agreeing(2);
    agreed_on_another_content.synced_content = Some(content("ours"));
    assert!(!are_kept_for(&agreed_on_another_content));
}

#[test]
fn the_tries_end_when_the_items_row_goes() {
    let mut tries = tries_of_a_download(&agreeing(2));
    tries.void_by_rows(&rows(&[&agreeing(3)]));
    assert_eq!(tries, LocalStepTries::default());
}

#[test]
fn the_tries_of_a_merge_end_when_either_row_changes() {
    let (mut local, onedrive) = (agreeing(2), agreeing(3));
    (local.onedrive_item, local.onedrive, local.onedrive_etag, local.synced_place, local.synced_content) = (None, None, None, None, None);
    let merge = LocalStep::of(&SyncStep::Row(RowStep::MergeItems { local: local.clone(), onedrive: onedrive.clone() })).expect("of the rows");
    let kept_for = |rows_now: BTreeMap<LocalItemId, ItemVersions>| {
        let mut tries = LocalStepTries::default();
        assert_eq!(tries.remember_failed_try(&merge, Reason::Failed, Some(60)), 1);
        assert_eq!(tries.list_items(), [id(2), id(3)].into(), "both rows are read");
        tries.void_by_rows(&rows_now);
        !tries.list_next_tries().is_empty()
    };
    assert!(kept_for(rows(&[&local, &onedrive])));
    let mut moved = onedrive.clone();
    moved.onedrive.as_mut().unwrap().place = Some(place("elsewhere"));
    assert!(!kept_for(rows(&[&local, &moved])), "the row of OneDrive's item is another");
    assert!(!kept_for(rows(&[&local])), "the row of OneDrive's item went");
    assert!(!kept_for(rows(&[&onedrive])), "the row of the local item went");
}

/// Another client renames an item back and forth, or changes its metadata: the eTag is
/// another one and the work is the same. The tries, and the wait, stay.
#[test]
fn an_etag_alone_that_changed_ends_no_tries() {
    let mut with_another_etag = agreeing(2);
    with_another_etag.onedrive_etag = Some("e2".to_owned());
    assert!(is_same_work(&agreeing(2), &with_another_etag));
    let mut tries = tries_of_a_download(&agreeing(2));
    tries.void_by_rows(&rows(&[&with_another_etag]));
    assert_eq!(tries.remember_failed_try(&download(&with_another_etag), Reason::Network, Some(240)), 3, "counted on");
}

/// An item both sides agreed on, removed in OneDrive.
/// Its row has no stamp and no eTag; the step that removes it here fails, and waits.
#[test]
fn the_step_for_an_item_onedrive_removed_waits_as_any_other() {
    let mut removed_in_onedrive = agreeing(2);
    (removed_in_onedrive.onedrive, removed_in_onedrive.onedrive_etag) = (None, None);
    let removal = LocalStep::of(&SyncStep::Folder(FolderStep::RemoveItem { item: removed_in_onedrive.clone() })).expect("towards the folder");
    let mut tries = LocalStepTries::default();
    for (count, next_try_at) in [(1, 60), (2, 120), (3, 240)] {
        assert_eq!(tries.remember_failed_try(&removal, Reason::Changed, Some(next_try_at)), count);
    }
    tries.void_by_rows(&rows(&[&removed_in_onedrive]));
    assert_eq!(tries.list_next_tries(), [((id(2), TriedStep::TowardsFolder(FolderStepKind::RemoveItem)), NextTry { not_before: 240, reason: Reason::Changed })].into());
}

/// A step that ends after the row changed gives its try with the versions it was planned
/// from: it is no try of the work the row is now, and is counted by itself.
#[test]
fn a_try_of_a_step_planned_from_other_versions_is_counted_from_one() {
    let mut tries = tries_of_a_download(&agreeing(2));
    let mut changed = agreeing(2);
    changed.onedrive.as_mut().unwrap().content = Some(content("theirs-2"));
    assert_eq!(tries.count_tries(&download(&changed)), 0);
    assert_eq!(tries.remember_failed_try(&download(&changed), Reason::Network, Some(60)), 1);
    assert_eq!(tries.count_tries(&download(&agreeing(2))), 0, "the tries kept are of the later work");
}

#[test]
fn the_tries_of_one_kind_of_step_are_not_those_of_another() {
    let planned_from = agreeing(2);
    let mut tries = tries_of_a_download(&planned_from);
    let take_content = LocalStep::of(&SyncStep::Folder(FolderStep::TakeOneDriveContent { item: planned_from.clone() })).expect("towards the folder");
    assert_eq!(tries.count_tries(&take_content), 0);
    assert_eq!(tries.remember_failed_try(&take_content, Reason::Failed, Some(30)), 1);
    assert_eq!(tries.list_next_tries().len(), 2);
    tries.forget_tries(&take_content);
    assert_eq!(tries.count_tries(&download(&planned_from)), 2, "the other step's stay");
}

#[test]
fn no_try_of_a_step_towards_onedrive_from_an_unsent_row_is_kept() {
    let mut unsent = agreeing(2);
    unsent.local.as_mut().unwrap().content = Some(content("mine"));
    unsent.unsent_since = Some(7);
    assert_eq!(LocalStep::of(&SyncStep::OneDrive(OneDriveStep::SendContent { item: unsent.clone() })), None, "the store keeps its tries, under the stamp");
    let described = LocalStep::describe(&SyncStep::OneDrive(OneDriveStep::SendContent { item: unsent }));
    assert_eq!((described.item(), described.kind()), (id(2), LocalStepKind::TowardsOneDrive), "a try the store could not be written is kept all the same");
    let hash = LocalStep::of(&SyncStep::Row(RowStep::HashLocalContent { item: agreeing(2) })).expect("of the rows");
    assert_eq!((hash.item(), hash.kind(), hash.kind().tried_step()), (id(2), LocalStepKind::OfRows(RowStepKind::HashLocalContent), TriedStep::OfRows(RowStepKind::HashLocalContent)));
}

/// A move owed to OneDrive after a killed exchange of names, and a content that is the
/// folder's own upload sent again: the row is not unsent, and the store keeps no try.
#[test]
fn the_tries_of_a_step_towards_onedrive_from_a_row_that_is_not_unsent_are_kept_and_told_as_a_sending() {
    let mut planned_from = agreeing(2);
    planned_from.onedrive.as_mut().unwrap().place = Some(place(".konedrive-temporary"));
    let send_move = SyncStep::OneDrive(OneDriveStep::SendMove { item: planned_from.clone(), parent: reference(1), name: OsString::from("report") });
    let send_content = SyncStep::OneDrive(OneDriveStep::SendContent { item: planned_from.clone() });
    for step in [send_move, send_content] {
        let kept_as = LocalStep::of(&step).expect("no stamp: the task keeps its tries");
        assert_eq!((kept_as.item(), kept_as.kind()), (id(2), LocalStepKind::TowardsOneDrive));
        let mut tries = LocalStepTries::default();
        assert_eq!(tries.remember_failed_try(&kept_as, Reason::Network, Some(60)), 1);
        assert_eq!(tries.remember_failed_try(&kept_as, Reason::Network, Some(120)), 2);
        assert_eq!(tries.list_next_tries(), [((id(2), TriedStep::TowardsOneDrive), NextTry { not_before: 120, reason: Reason::Network })].into(), "every step towards OneDrive of the item waits");
        // OneDrive confirmed the move: the row is another piece of work.
        let mut confirmed = planned_from.clone();
        confirmed.onedrive.as_mut().unwrap().place = Some(place("report"));
        tries.void_by_rows(&rows(&[&confirmed]));
        assert_eq!(tries, LocalStepTries::default());
    }
}

#[test]
fn a_refresh_makes_every_try_due_and_keeps_its_count() {
    let planned_from = agreeing(2);
    let mut tries = tries_of_a_download(&planned_from);
    assert_eq!(tries.remember_failed_try(&download(&agreeing(3)), Reason::Failed, None), 1);
    assert_eq!(tries.clear_next_tries(), 1);
    assert_eq!(tries.list_next_tries(), BTreeMap::new());
    assert_eq!(tries.remember_failed_try(&download(&planned_from), Reason::Network, Some(240)), 3);
}
