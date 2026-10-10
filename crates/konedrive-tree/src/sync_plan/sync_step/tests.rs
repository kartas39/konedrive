use super::*;
use crate::sync_plan::tests::{hashed, id, made_file, moved_here, reference, synced_file, with_content_in_onedrive, ROOT};

#[test]
fn the_versions_a_step_was_planned_from_are_those_of_the_row_until_one_of_them_changes() {
    let row = synced_file(2, ROOT, "a.txt");
    let planned_from = ItemVersions::of(&row);
    assert!(planned_from.are_those_of(&row));

    // What is no version is not asked: whether the content is on disk, the stamps.
    let mut same_versions = row.clone();
    same_versions.content_presence = crate::model::ContentPresence::OnDisk;
    same_versions.confirmed_at_count = 9;
    same_versions.unsent_since = Some(4);
    assert!(planned_from.are_those_of(&same_versions));

    assert!(!planned_from.are_those_of(&moved_here(row.clone(), ROOT, "b.txt")), "the local version");
    assert!(!planned_from.are_those_of(&with_content_in_onedrive(row.clone(), hashed("theirs"))), "the OneDrive version");
    let mut other = row.clone();
    other.onedrive_etag = Some("e-other".to_owned());
    assert!(!planned_from.are_those_of(&other), "the eTag");
    let mut other = row.clone();
    other.synced_content = Some(hashed("other"));
    assert!(!planned_from.are_those_of(&other), "the synced content");
    let mut other = row.clone();
    other.synced_place = None;
    assert!(!planned_from.are_those_of(&other), "the synced place");
    let mut other = row.clone();
    other.onedrive_item = Some(reference("other"));
    assert!(!planned_from.are_those_of(&other), "the binding");
    let mut other = row;
    other.local_item_id = id(3);
    assert!(!planned_from.are_those_of(&other), "another item");
}

#[test]
fn a_step_is_of_the_item_whose_versions_it_carries_and_a_merge_of_the_item_the_folder_shows() {
    let (here, in_onedrive) = (ItemVersions::of(&made_file(2, ROOT, "a.txt")), ItemVersions::of(&synced_file(3, ROOT, "a.txt")));
    assert_eq!(SyncStep::OneDrive(OneDriveStep::SendContent { item: in_onedrive.clone() }).local_item_id(), id(3));
    assert_eq!(SyncStep::Folder(FolderStep::RenameToConflictCopy { item: here.clone(), name: "a-fedora.txt".into() }).local_item_id(), id(2));
    assert_eq!(SyncStep::Row(RowStep::MergeItems { local: here, onedrive: in_onedrive }).local_item_id(), id(2));
}
