//! The table "its content": a test per cell, and per row C1–C11 of the map.

use konedrive_reason::Reason;

use super::*;

// -- same here, changed in OneDrive

/// C1. Today: `a_file_that_stayed_with_new_content_loses_its_downloaded_copy`
/// (`fuse/cycle_step/cycle_actions/tests.rs`),
/// `a_downloaded_copy_whose_content_is_stale_is_removed` and
/// `a_copy_whose_writer_closed_it_is_removed_when_its_file_has_new_content`
/// (`fuse/cycle_step/tests.rs`).
#[test]
fn a_clean_copy_whose_content_onedrive_changed_goes_and_the_file_shows_onedrives_content() {
    let given = input(vec![with_content_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), hashed("theirs"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 2) }));
}

/// C2. Today: the scenario `pinned_file_stays_downloaded` and
/// `a_pinned_file_that_is_not_downloaded_is_to_download`
/// (`fuse/pinned_file_downloader/tests.rs`). Two steps, in two plans: the copy goes, and
/// the plan made after it downloads the content again.
#[test]
fn a_pinned_file_whose_content_onedrive_changed_loses_its_copy_and_is_then_downloaded_again() {
    let mut given = input(vec![with_content_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), hashed("theirs"))]);
    given.pinned_items = [id(2)].into();
    assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 2) }));

    // What the step leaves: OneDrive's content on every side, and not on disk.
    let with_onedrives_content = |number: i64, name: &str| {
        let mut file = synced_file(number, ROOT, name);
        for content in [&mut file.local.as_mut().unwrap().content, &mut file.onedrive.as_mut().unwrap().content, &mut file.synced_content] {
            *content = Some(hashed("theirs"));
        }
        file
    };
    let mut given = input(vec![with_onedrives_content(2, "a.txt"), on_disk(with_onedrives_content(3, "b.txt"))]);
    given.pinned_items = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), folder_step(FolderStep::DownloadContent { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), Answer::Nothing, "a pinned file whose content is on disk");
    given.pinned_items.clear();
    assert_eq!(answer(&given, 2), Answer::Nothing, "a file with no pin stays not on disk");
}

/// C3. Today: `a_file_that_stayed_is_announced_as_shown_anew`
/// (`fuse/cycle_step/cycle_actions/tests.rs`). The step is the one of C1: that there is no
/// copy to remove is read off the row at its commit.
#[test]
fn a_file_not_on_disk_whose_content_onedrive_changed_shows_onedrives_content() {
    let given = input(vec![with_content_in_onedrive(synced_file(2, ROOT, "a.txt"), hashed("theirs"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 2) }));
}

/// C4 and C5. While a program holds the file open for writing nothing is planned for
/// it, whatever OneDrive changed. Closed unchanged, the file follows OneDrive; one rule,
/// whatever the folder's access mode.
#[test]
fn a_file_held_open_for_writing_whose_content_onedrive_changed_is_not_planned_until_it_is_closed() {
    // Open: the row has the content of the open, and what it was before.
    let mut open = with_content_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), written_here()), hashed("theirs"));
    open.content_before_write = Some(hashed("h2"));
    let closed_unchanged = with_content_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), hashed("theirs"));
    {
        let mut given = input(vec![open.clone()]);
        given.items_open_for_writing = [id(2)].into();
        assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));

        let given = input(vec![closed_unchanged.clone()]);
        assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 2) }));
    }
}

// -- changed here, same in OneDrive

/// C6. Today: `edits_and_renames_go_up_guarded` (`upload/tests/row_kinds.rs`), the scenario
/// `edit_of_downloaded_file_is_uploaded`. No hash is asked before a content a program
/// changed is sent, as today; a content hashed here that is another is sent the same.
#[test]
fn content_changed_here_is_sent() {
    for content in [written_here(), hashed_here("mine")] {
        let given = input(vec![with_content_here(synced_file(2, ROOT, "a.txt"), content)]);
        assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 2) }));
        assert_eq!(versions(&given, 2).onedrive_etag.as_deref(), Some("e2"));
    }
}

/// C9. Today: `conflicts_keep_both_and_the_first_rename_wins` (`upload/tests/conflicts.rs`,
/// "edit × rename there"): OneDrive's eTag moved and its content is the one agreed on. The
/// content is sent with the fresh eTag, which is the row's; where OneDrive renamed the
/// item, the folder follows first.
#[test]
fn content_changed_here_is_sent_with_the_etag_onedrive_has_now() {
    let mut etag_moved = with_content_here(synced_file(2, ROOT, "a.txt"), written_here());
    etag_moved.onedrive_etag = Some("e-fresh".to_owned());
    let given = input(vec![etag_moved, moved_in_onedrive(with_content_here(synced_file(3, ROOT, "b.txt"), written_here()), ROOT, "c.txt")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 2) }));
    assert_eq!(versions(&given, 2).onedrive_etag.as_deref(), Some("e-fresh"));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
}

/// C10: the content is sent into the item where OneDrive has it, and the item stays
/// here.
#[test]
fn content_changed_here_is_sent_into_an_item_the_folder_cannot_show_where_onedrive_has_it() {
    let mut given = input(vec![synced_directory(3, ROOT, "vault"), moved_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), written_here()), id(3), "a.txt")]);
    given.folder_visibility.insert(id(2), FolderVisibility::CannotBeShown);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 2) }));
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
}

/// Open: what becomes of C10's item once its content is sent. The design's cell says "the
/// item staying here", and the rows after the upload are those of P6: place the same here,
/// moved in OneDrive to where the folder cannot show it, content the same. Reading 1
/// (here): the plan is a function of the rows, so the item then goes here as if removed,
/// its copy clean. Reading 2 (today: the base keeps the disk's place, and no cycle looks at
/// the item again): it stays for good, which the rows cannot say without a field more.
#[test]
fn open_an_item_the_folder_cannot_show_goes_here_once_its_content_was_sent() {
    // What the upload's confirmation leaves: one content on every side.
    let mut sent = moved_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), id(3), "a.txt");
    for content in [&mut sent.local.as_mut().unwrap().content, &mut sent.onedrive.as_mut().unwrap().content, &mut sent.synced_content] {
        *content = Some(hashed("mine"));
    }
    let mut given = input(vec![synced_directory(3, ROOT, "vault"), sent]);
    given.folder_visibility.insert(id(2), FolderVisibility::CannotBeShown);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));
}

// -- changed on both sides

/// C7. Today: none by name (`every_crash_point_is_replayed_to_the_same_end`,
/// `upload/tests/row_kinds.rs`, passes through it). Differs from today in when it is known:
/// the hash is in the row, and nothing is sent.
#[test]
fn content_changed_to_the_same_on_both_sides_agrees() {
    let given = input(vec![with_content_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), hashed_here("both")), hashed("both"))]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnContent { item: versions(&given, 2) }));
}

/// C8. Today: `conflicts_keep_both_and_the_first_rename_wins` (`upload/tests/conflicts.rs`,
/// "edit × edit"), `an_edit_of_an_outdated_download_is_guarded_by_its_ctag`, the scenarios
/// `file_changed_in_onedrive_before_the_upload_keeps_both` and
/// `file_changed_in_onedrive_while_a_change_waits_keeps_both`. The step gives up the
/// binding; the conflict copy's name is given between items, where the item that took the
/// binding comes to the name. Another size is another content with no hash asked.
#[test]
fn content_changed_to_another_on_each_side_keeps_both_the_local_one_giving_up_its_binding() {
    let mut longer = written_here();
    if let ContentVersion::ChangedHere { size, .. } = &mut longer {
        *size = 4;
    }
    for content in [hashed_here("mine"), longer] {
        let given = input(vec![with_content_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), content), hashed("theirs"))]);
        assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    }
}

/// C11. Differs from today, as the design says: today the hash is computed inside the
/// upload's step after OneDrive refused, and kept nowhere (`upload/content/session.rs:171-175`);
/// here it is a step of its own, asked only where the question is "is it the same
/// content?", and the plan made after it has the answer in the row (C7, C8).
#[test]
fn content_changed_on_both_sides_with_no_hash_known_here_is_hashed_first() {
    let given = input(vec![with_content_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), written_here()), hashed("theirs"))]);
    assert_eq!(answer(&given, 2), row_step(RowStep::HashLocalContent { item: versions(&given, 2) }));
}

/// C11's edge: OneDrive gives no hash for some files. Hashing the local content cannot
/// tell then, so no hash is asked and both are kept. New.
#[test]
fn content_changed_on_both_sides_where_onedrive_gives_no_hash_keeps_both() {
    let no_hash = ContentVersion::Hashed { size: 3, mtime: 300, mtime_nsec: 0, hash: None, ctag: Some("c-theirs".to_owned()) };
    let given = input(vec![with_content_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), written_here()), no_hash)]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
}

// -- same on both sides, by the bytes

/// New (the notes of part 2, N4): OneDrive changed the cTag or the time of a content whose
/// hash is the one agreed on. The bytes are the same, so no copy goes; the content versions
/// are made one, or the row would be read as changed in OneDrive for good.
#[test]
fn content_whose_bytes_are_the_same_and_whose_versions_are_not_one_agrees() {
    let retagged = ContentVersion::Hashed { size: 3, mtime: 500, mtime_nsec: 0, hash: Some("h2".to_owned()), ctag: Some("c-retagged".to_owned()) };
    let given = input(vec![with_content_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")), retagged)]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnContent { item: versions(&given, 2) }));

    // A program wrote the bytes the file had: hashed, the local content is the synced one.
    let given = input(vec![with_content_here(synced_file(2, ROOT, "a.txt"), hashed_here("h2"))]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnContent { item: versions(&given, 2) }));
}

/// Stage 0b's §2.1: the time of a clean copy was set here (`touch`), which writes the time
/// into the local content and the content agreed on; OneDrive's keeps OneDrive's time. The
/// three are one content but for the time, and the time here stays local: nothing is to
/// do, on disk or not, and a pin still downloads the file. The same row is what a time
/// OneDrive changed alone leaves: it is not taken over either.
#[test]
fn a_clean_copy_whose_time_alone_is_not_onedrives_is_left_as_it_is() {
    let touched = |mut file: Item| {
        let with_time = ContentVersion::Hashed { size: 3, mtime: 5000, mtime_nsec: 7, hash: Some("h2".to_owned()), ctag: Some("c-h2".to_owned()) };
        file.local.as_mut().unwrap().content = Some(with_time.clone());
        file.synced_content = Some(with_time);
        file
    };
    for file in [touched(on_disk(synced_file(2, ROOT, "a.txt"))), touched(synced_file(2, ROOT, "a.txt"))] {
        let mut given = input(vec![file.clone()]);
        assert_eq!(answer(&given, 2), Answer::Nothing, "{:?}", file.content_presence);
        given.pinned_items = [id(2)].into();
        let pinned = if file.content_presence == ContentPresence::OnDisk { Answer::Nothing } else { folder_step(FolderStep::DownloadContent { item: versions(&given, 2) }) };
        assert_eq!(answer(&given, 2), pinned);
    }
    // Where more than the time differs, the versions are made one as before.
    let mut retagged = touched(on_disk(synced_file(2, ROOT, "a.txt")));
    retagged.onedrive.as_mut().unwrap().content = Some(ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some("h2".to_owned()), ctag: Some("c-retagged".to_owned()) });
    let given = input(vec![retagged]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnContent { item: versions(&given, 2) }));
}

/// New: a file the folder shows and OneDrive has in one row, with nothing agreed on (two
/// new files that were merged; a file the start-up check bound by its mark). The content
/// table's last cell holds: the same hash agrees, another keeps both, none is hashed first.
#[test]
fn a_file_both_sides_have_with_nothing_agreed_on_is_compared_by_its_hash() {
    let nothing_agreed_on = |here: ContentVersion| {
        let mut file = with_content_here(synced_file(2, ROOT, "a.txt"), here);
        file.synced_place = None;
        file.synced_content = None;
        input(vec![file])
    };
    // The place is agreed on first.
    let given = nothing_agreed_on(hashed_here("h2"));
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnPlace { item: versions(&given, 2) }));

    let place_agreed_on = |here: ContentVersion| {
        let mut given = nothing_agreed_on(here);
        given.items[1].synced_place = Some(place(ROOT, "a.txt"));
        given
    };
    let given = place_agreed_on(hashed_here("h2"));
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnContent { item: versions(&given, 2) }));
    let given = place_agreed_on(hashed_here("mine"));
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    let given = place_agreed_on(written_here());
    assert_eq!(answer(&given, 2), row_step(RowStep::HashLocalContent { item: versions(&given, 2) }));
}
