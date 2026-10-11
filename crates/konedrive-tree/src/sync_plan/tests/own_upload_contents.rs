//! The plan for a file whose content in OneDrive is what an upload of this folder left
//! there in place of what it sent ([`SyncPlanInput::own_upload_contents`]): no change of
//! OneDrive's. The local content stands and is sent again; the file does not take
//! OneDrive's, no conflict copy is made against it, and a file removed here does not come
//! back for it.

use super::*;
use crate::OwnUploadContent;

/// The content the upload left in OneDrive.
fn left() -> ContentVersion {
    hashed("left")
}

/// The fact for the item `number`, whose content `onedrive_content` in OneDrive the upload
/// left.
fn own_upload(number: i64, onedrive_content: ContentVersion) -> (LocalItemId, OwnUploadContent) {
    (id(number), OwnUploadContent { onedrive_content, local_content: written_here() })
}

/// The third column of the content table (`OneDriveContentOrigin`), a row per thing done
/// here: the content written, the content written and hashed, and the content put back to
/// the one agreed on. Not told, each is planned as a change of OneDrive's.
#[test]
fn a_content_the_folders_own_upload_left_in_onedrive_is_sent_again() {
    let file = |number: i64, name: &str| with_content_in_onedrive(synced_file(number, ROOT, name), left());
    let mut given = input(vec![with_content_here(file(2, "written.txt"), written_here()), with_content_here(file(3, "hashed.txt"), hashed_here("mine")), on_disk(file(4, "put-back.txt"))]);
    assert_eq!(answer(&given, 2), row_step(RowStep::HashLocalContent { item: versions(&given, 2) }), "not told: changed on both sides");
    assert_eq!(answer(&given, 3), row_step(RowStep::GiveUpBinding { item: versions(&given, 3) }), "not told: both are kept");
    assert_eq!(answer(&given, 4), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 4) }), "not told: OneDrive's is taken");

    given.own_upload_contents = [own_upload(2, left()), own_upload(3, left()), own_upload(4, left())].into();
    for number in [2, 3, 4] {
        assert_eq!(answer(&given, number), onedrive_step(OneDriveStep::SendContent { item: versions(&given, number) }), "item {number}");
    }
    // What keeps a content from being sent keeps this one too.
    given.is_onedrive_full = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::WaitingForSpace)));
}

/// The fact counts only while the row's OneDrive content is the one it names: once a
/// listing wrote another, OneDrive changed the file. And where OneDrive has the content
/// agreed on, nothing is left of the upload.
#[test]
fn a_fact_of_another_content_than_onedrive_has_says_nothing() {
    let mut given = input(vec![with_content_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), hashed("theirs")), on_disk(synced_file(3, ROOT, "b.txt"))]);
    given.own_upload_contents = [own_upload(2, left()), own_upload(3, left())].into();
    assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), Answer::Nothing);
}

/// A file removed here does not come back for the content its own upload left: its removal
/// is sent. Not told, it comes back as a file OneDrive changed.
#[test]
fn a_file_removed_here_does_not_come_back_for_the_content_its_own_upload_left() {
    let mut given = input(vec![with_content_in_onedrive(removed_here(synced_file(2, ROOT, "a.txt")), left())]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }), "not told");
    given.own_upload_contents = [own_upload(2, left())].into();
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 2) }));
}

/// The review's M4: `rm -r` of a directory (2) with such a file in it (3). The file is
/// removed with the directory; not told, the removal is of the folder alone, and the
/// directory the user removed is shown again.
#[test]
fn a_directory_is_removed_with_a_file_whose_content_in_onedrive_is_the_own_upload() {
    let mut given = input(vec![removed_here(synced_directory(2, ROOT, "d")), with_content_in_onedrive(removed_here(synced_file(3, id(2), "f")), left())]);
    assert_eq!(answer(&given, 2), row_step(RowStep::ForgetSyncedVersion { item: versions(&given, 2) }), "not told");
    given.own_upload_contents = [own_upload(3, left())].into();
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 3)] }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))));
}
