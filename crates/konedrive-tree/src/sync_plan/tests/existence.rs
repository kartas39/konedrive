//! The table "whether the item exists" of `docs/design/sync.md` §6.3: a test per cell.

use konedrive_reason::Reason;

use super::*;

// -- is there here, removed in OneDrive

#[test]
fn a_file_not_on_disk_that_onedrive_removed_goes_here() {
    let given = input(vec![removed_in_onedrive(synced_file(2, ROOT, "a.txt"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));
}

/// The step is the one of a file not on disk: whether a copy goes with the name is read
/// off the row at its commit.
#[test]
fn a_file_with_a_clean_copy_that_onedrive_removed_goes_here_its_copy_with_it() {
    let given = input(vec![removed_in_onedrive(on_disk(synced_file(2, ROOT, "a.txt")))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));
}

/// Read as "content changed here" once the file is closed (the scenario
/// `file_removed_in_onedrive_while_changed_here_is_uploaded_again`,
/// `tests/fuse-probe/file_writes.sh`); one rule, whatever the folder's state of sending.
#[test]
fn a_file_held_open_for_writing_that_onedrive_removed_is_not_planned_until_it_is_closed() {
    let file = removed_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), written_here()));
    {
        let mut given = input(vec![file.clone()]);
        given.items_open_for_writing = [id(2)].into();
        assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));
        // Closed changed: the cell of a file changed here that OneDrive removed.
        given.items_open_for_writing.clear();
        assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    }
}

/// While the folder shows something in the directory it
/// waits for that item, deepest first: what becomes of a directory that holds something of
/// the user's own is decided between items.
#[test]
fn a_directory_onedrive_removed_goes_here_once_the_folder_shows_nothing_in_it() {
    let given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));

    let given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d")), removed_in_onedrive(synced_directory(3, id(2), "e")), removed_in_onedrive(synced_file(4, id(3), "a.txt"))]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::AnotherItem(id(3))));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(4))));
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RemoveItem { item: versions(&given, 4) }));
}

/// No hash is asked: a content a program changed is kept.
#[test]
fn a_file_changed_here_that_onedrive_removed_gives_up_its_binding_and_is_sent_again() {
    for content in [written_here(), hashed_here("other")] {
        let given = input(vec![removed_in_onedrive(with_content_here(synced_file(2, ROOT, "a.txt"), content))]);
        assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    }
    // The plan made after the step: the item is new here, at the name it had.
    let mut sent_again = made_file(2, ROOT, "a.txt");
    sent_again.unsent_since = Some(1);
    let given = input(vec![sent_again]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendNewFile { item: versions(&given, 2), parent: reference("root"), name: "a.txt".into() }));
}

#[test]
fn a_file_on_disk_moved_here_that_onedrive_removed_gives_up_its_binding_and_is_sent_again() {
    let given = input(vec![removed_in_onedrive(on_disk(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt")))]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
}

/// A directory moved here that OneDrive removed goes, unless something of the user's own is
/// below it. A directory the folder shows something in waits for it, as one not moved does.
#[test]
fn a_directory_moved_here_that_onedrive_removed_goes_here_once_the_folder_shows_nothing_in_it() {
    let given = input(vec![removed_in_onedrive(moved_here(synced_directory(2, ROOT, "d"), ROOT, "e"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));

    // Something of the user's own in it: it stays and is sent again, as between items.
    let given = input(vec![removed_in_onedrive(moved_here(synced_directory(2, ROOT, "d"), ROOT, "e")), made_file(3, id(2), "mine.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))));
}

/// A new item, or a move, into a directory that has a binding and that OneDrive does not
/// have (it removed it; or no listing told of it yet) is not sent into that binding: the
/// item waits for the directory, as for one that has no binding. A directory OneDrive removed gives up
/// its binding when all the folder shows in it waits for it so (between items:
/// `a_directory_onedrive_removed_that_holds_something_of_the_users_own_stays_and_is_sent_again`),
/// and is then made again.
#[test]
fn nothing_is_sent_into_a_directory_onedrive_does_not_have() {
    let mut never_listed = made_directory(6, ROOT, "marked");
    never_listed.onedrive_item = Some(reference("I6"));
    let given = input(vec![
        removed_in_onedrive(synced_directory(2, ROOT, "d")),
        made_file(3, id(2), "mine.txt"),
        made_directory(4, id(2), "mine"),
        moved_here(synced_file(5, ROOT, "a.txt"), id(2), "a.txt"),
        never_listed,
        made_file(7, id(6), "b.txt"),
    ]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    for sent_into_it in [3, 4, 5] {
        assert_eq!(answer(&given, sent_into_it), Answer::Wait(WaitReason::AnotherItem(id(2))), "item {sent_into_it}");
    }
    assert_eq!(answer(&given, 7), Answer::Wait(WaitReason::AnotherItem(id(6))));
}

#[test]
fn a_file_not_on_disk_moved_here_that_onedrive_removed_goes_here() {
    let given = input(vec![removed_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RemoveItem { item: versions(&given, 2) }));
}

/// What the rows say OneDrive removed waits while a listing is under way
/// (`docs/design/sync.md` §6.3).
#[test]
fn what_the_rows_say_onedrive_removed_waits_for_a_listing_under_way_to_end() {
    let mut given = input(vec![
        removed_in_onedrive(synced_file(2, ROOT, "a.txt")),
        removed_in_onedrive(with_content_here(synced_file(3, ROOT, "b.txt"), written_here())),
        removed_in_onedrive(synced_directory(4, ROOT, "d")),
        removed_in_onedrive(removed_here(synced_file(5, ROOT, "c.txt"))),
    ]);
    given.is_listing_under_way = true;
    for item in 2..=5 {
        assert_eq!(answer(&given, item), Answer::Wait(WaitReason::Listing), "item {item}");
    }
}

// -- is there here, never was in OneDrive

#[test]
fn a_file_made_here_is_sent_as_a_new_file() {
    let given = input(vec![synced_directory(2, ROOT, "d"), made_file(3, id(2), "new.txt")]);
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendNewFile { item: versions(&given, 3), parent: reference("I2"), name: "new.txt".into() }));
}

#[test]
fn a_directory_made_here_is_made_in_onedrive() {
    let given = input(vec![made_directory(2, ROOT, "d")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 2), parent: reference("root"), name: "d".into() }));
}

/// A parent not in OneDrive yet: the directory is made first. The step carries the binding of the directory, so it waits
/// for the directory to have one; and for a listing while the root itself has none.
#[test]
fn an_item_made_here_waits_for_its_directory_to_be_in_onedrive() {
    let given = input(vec![made_directory(2, ROOT, "d"), made_file(3, id(2), "new.txt"), made_directory(4, id(2), "e")]);
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::AnotherItem(id(2))));

    // A directory whose row was not given is said by its id, for the caller to give.
    let given = input(vec![made_file(3, id(2), "new.txt")]);
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::ItemNotGiven(id(2))));

    // The root itself has no binding while the first listing is under way.
    let mut given = input(vec![made_file(3, ROOT, "new.txt")]);
    given.items[0].onedrive_item = None;
    given.items[0].onedrive = None;
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing));
}

/// Open: an item the folder shows that has a binding, of which no listing said what
/// OneDrive has, and nothing agreed on: what the start-up check makes of a file marked with
/// an id no row has (`docs/design/mount.md` §7.4, the plan compares the two).
/// Reading 1 (here): while a listing is under way it waits for it; with none under way
/// OneDrive does not have the item, the binding is given up and the item is sent as new.
/// Reading 2: it waits for a listing always, since a listing of changes that names nothing
/// of the item does not say OneDrive lacks it; the item is then never sent if OneDrive
/// lacks it.
#[test]
fn open_an_item_bound_to_what_no_listing_told_of_waits_for_a_listing_and_then_gives_up_its_binding() {
    let mut never_listed = made_file(2, ROOT, "a.txt");
    never_listed.onedrive_item = Some(reference("I2"));
    let mut given = input(vec![never_listed]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
}

// -- removed here, is there in OneDrive

/// The guard is the eTag the step carries with the versions.
#[test]
fn the_removal_of_a_file_is_sent_guarded_by_its_etag() {
    let given = input(vec![removed_here(synced_file(2, ROOT, "a.txt"))]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 2) }));
    assert_eq!(versions(&given, 2).onedrive_etag.as_deref(), Some("e2"));
}

/// The scenario `file_changed_in_onedrive_while_removed_here_comes_back`: the rows say it
/// beforehand and nothing is sent.
#[test]
fn a_file_removed_here_whose_content_onedrive_changed_comes_back() {
    let given = input(vec![with_content_in_onedrive(removed_here(synced_file(2, ROOT, "a.txt")), hashed("theirs"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }));
}

#[test]
fn the_removal_of_a_file_onedrive_only_moved_is_sent() {
    let given = input(vec![synced_directory(3, ROOT, "d"), moved_in_onedrive(removed_here(synced_file(2, ROOT, "a.txt")), id(3), "b.txt")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 2) }));
    assert_eq!(versions(&given, 2).onedrive_etag.as_deref(), Some("e-moved"));
}

/// Unguarded, it stands whatever OneDrive did to the directory since; what goes with it
/// is decided between items.
#[test]
fn the_removal_of_a_directory_is_sent_unguarded_and_stands() {
    let given = input(vec![removed_here(synced_directory(2, ROOT, "d")), synced_directory(4, ROOT, "e"), moved_in_onedrive(removed_here(synced_directory(3, ROOT, "f")), id(4), "g")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![] }));
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 3), below: vec![] }));
}

/// A file removed here whose content OneDrive changed comes back; where the folder cannot
/// show it, what was agreed on is forgotten, a step
/// of the rows alone, so that the row is that of an item of OneDrive the folder cannot
/// show, and no row stays unsent for good. While a listing is under way it waits.
#[test]
fn a_file_removed_here_whose_changed_content_the_folder_cannot_show_forgets_what_was_agreed_on() {
    let mut given = input(vec![with_content_in_onedrive(removed_here(synced_file(2, ROOT, "a.txt")), hashed("theirs")), with_content_in_onedrive(removed_here(synced_file(3, ROOT, "b.txt")), hashed("theirs"))]);
    given.folder_visibility.insert(id(2), FolderVisibility::CannotBeShown);
    given.items[2].onedrive.as_mut().unwrap().place = Some(place(ROOT, "b/c"));
    assert_eq!(answer(&given, 2), row_step(RowStep::ForgetSyncedVersion { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), row_step(RowStep::ForgetSyncedVersion { item: versions(&given, 3) }), "a name that is no name of a directory entry");
    // The row the step leaves is left out, and waits for nothing.
    let mut forgotten = given.clone();
    forgotten.items[1].synced_place = None;
    forgotten.items[1].synced_content = None;
    forgotten.items[1].unsent_since = None;
    assert_eq!(answer(&forgotten, 2), Answer::Exclusion(ExclusionReason::CannotBeShown));
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
}

// -- removed on both sides

/// The row is left with its binding alone; the guard against an
/// outdated listing deletes it.
#[test]
fn an_item_both_sides_removed_has_its_row_dropped() {
    let given = input(vec![removed_in_onedrive(removed_here(synced_file(2, ROOT, "a.txt"))), removed_in_onedrive(removed_here(synced_directory(3, ROOT, "d")))]);
    assert_eq!(answer(&given, 2), row_step(RowStep::AgreeOnRemoval { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), row_step(RowStep::AgreeOnRemoval { item: versions(&given, 3) }));
}

// -- never was here, is there in OneDrive

#[test]
fn an_item_new_in_onedrive_appears_here() {
    let given = input(vec![listed(2, ItemKind::File, ROOT, "a.txt"), listed(3, ItemKind::Directory, ROOT, "d")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::ShowItem { item: versions(&given, 3) }));
}

/// An item new in OneDrive, between it and its neighbours: it appears in a
/// directory the folder shows, at a name nothing has; until then it waits for the
/// directory, or for the item that has the name. What becomes of the item in the way is
/// decided between items.
#[test]
fn an_item_new_in_onedrive_waits_for_its_directory_and_for_its_name_here() {
    let given = input(vec![listed(2, ItemKind::Directory, ROOT, "d"), listed(3, ItemKind::File, id(2), "a.txt"), made_file(4, ROOT, "b.txt"), listed(5, ItemKind::File, ROOT, "b.txt")]);
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::AnotherItem(id(2))));
    assert_eq!(answer(&given, 5), Answer::Wait(WaitReason::AnotherItem(id(4))));
    // Another case of letters is another name in the folder.
    let given = input(vec![made_file(4, ROOT, "B.txt"), listed(5, ItemKind::File, ROOT, "b.txt")]);
    assert_eq!(answer(&given, 5), folder_step(FolderStep::ShowItem { item: versions(&given, 5) }));
}

/// Also a name of OneDrive that is no name of a directory entry.
#[test]
fn an_item_new_in_onedrive_that_the_folder_cannot_show_does_not_appear() {
    let mut given = input(vec![listed(2, ItemKind::File, ROOT, "a.txt"), listed(3, ItemKind::File, ROOT, "a/b")]);
    given.folder_visibility.insert(id(2), FolderVisibility::CannotBeShown);
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::CannotBeShown));
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::CannotBeShown));
}

/// An item whose side in OneDrive cannot be judged yet waits for the listing under way.
#[test]
fn an_item_new_in_onedrive_that_cannot_be_judged_yet_waits_for_the_listing_to_end() {
    let mut given = input(vec![listed(2, ItemKind::File, ROOT, "a.txt"), listed(3, ItemKind::File, ROOT, "b.txt"), listed(4, ItemKind::File, ROOT, "c.txt")]);
    given.is_listing_under_way = true;
    given.folder_visibility.insert(id(2), FolderVisibility::NotKnownYet);
    given.folder_visibility.insert(id(3), FolderVisibility::CannotBeShown);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing), "a later page may say it can be shown");
    assert_eq!(answer(&given, 4), folder_step(FolderStep::ShowItem { item: versions(&given, 4) }), "what can be shown appears while the listing goes on");
    // An item OneDrive has that the caller said nothing of is not known yet.
    given.folder_visibility.remove(&id(4));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::Listing));
}

// -- never was on either side; removed here, never was in OneDrive

/// An item made and removed here before it was sent has no row (the removal deletes it),
/// so the plan has nothing to cancel. The
/// rows that are left with no version on either side are no items: a directory mentioned
/// by a listing before it was listed, and a removal OneDrive confirmed, kept for the guard
/// against an outdated listing.
#[test]
fn a_row_with_no_version_on_either_side_is_no_item_and_is_not_planned() {
    let mut binding_alone = blank(2, ItemKind::Directory);
    binding_alone.onedrive_item = Some(reference("I2"));
    let mut given = input(vec![binding_alone, blank(3, ItemKind::File)]);
    given.items_with_running_step = [id(2)].into();
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Nothing);
    assert_eq!(answer(&given, 3), Answer::Nothing);
}
