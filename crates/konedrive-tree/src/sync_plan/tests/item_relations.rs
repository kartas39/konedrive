//! What the plan decides between items: a test per case of the design's list
//! (`docs/tasks/item-versions-design.md`, §6, "what it decides between items"), the guard
//! against a mass removal, the closed set, the order of the steps and the rule that a plan
//! in which nothing can run says why. A test's comment names the row of the map of today's
//! code (`docs/tasks/item-versions-plan-map.txt`, §2: B1a–B8), or says "new" or "differs
//! from today".

use konedrive_reason::Reason;

use super::*;

/// The temporary name the item `number` goes through.
fn temporary_name(number: i64) -> std::ffi::OsString {
    format!(".konedrive-swap-{number}").into()
}

fn waits_for(number: i64) -> Answer {
    Answer::Wait(WaitReason::AnotherItem(id(number)))
}

// -- names

/// B1a. Today: rule 4 of `outbox/pick.rs` (`a_taker_waits_for_what_frees_its_name`,
/// `outbox/pick/tests.rs`): a taker waits for every freer of its name, lower-cased.
#[test]
fn what_frees_a_name_in_onedrive_goes_before_what_takes_it() {
    let given = input(vec![
        moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"),
        made_file(3, ROOT, "a.txt"),
        removed_here(synced_directory(4, ROOT, "c")),
        made_directory(5, ROOT, "C"),
    ]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "b.txt".into() }));
    assert_eq!(answer(&given, 3), waits_for(2), "the move frees the name");
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 4), below: Vec::new() }));
    assert_eq!(answer(&given, 5), waits_for(4), "the removal frees the name, as OneDrive compares names");
}

/// B1b: no rule today, where it is safe only by the marks on disk
/// (`a_file_moved_onto_a_marked_copy_is_not_moved`, `fuse/cycle_step/cycle_actions/tests.rs`).
#[test]
fn what_frees_a_name_in_the_folder_goes_before_what_takes_it() {
    let given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), listed(3, ItemKind::File, ROOT, "a.txt"), removed_in_onedrive(synced_file(4, ROOT, "c.txt")), moved_in_onedrive(synced_file(5, ROOT, "d.txt"), ROOT, "c.txt")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RemoveItem { item: versions(&given, 4) }));
    assert_eq!(answer(&given, 5), waits_for(4));
}

/// B2a: differs from today, which sends the taker, is answered 409 and only then goes
/// through `.konedrive-swap-<id>` (`two_files_exchange_names_through_a_temporary_name`,
/// `upload/tests/temporary_names.rs`). The plan foresees it: of the items that wait for
/// each other, the one with the lowest id is sent to a temporary name.
#[test]
fn items_that_exchange_names_here_go_through_a_temporary_name_in_onedrive() {
    let given = input(vec![moved_here(synced_file(2, ROOT, "x"), ROOT, "y"), moved_here(synced_file(3, ROOT, "y"), ROOT, "x")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: temporary_name(2) }));
    assert_eq!(answer(&given, 3), waits_for(2));

    // Once OneDrive has the first under the temporary name, which is then the place agreed
    // on, the second is sent, and the first after it.
    let mut first = moved_here(synced_file(2, ROOT, "x"), ROOT, "y");
    first.onedrive.as_mut().unwrap().place = Some(place(ROOT, ".konedrive-swap-2"));
    first.synced_place = Some(place(ROOT, ".konedrive-swap-2"));
    let given = input(vec![first, moved_here(synced_file(3, ROOT, "y"), ROOT, "x")]);
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 3), parent: reference("root"), name: "x".into() }));
    assert_eq!(answer(&given, 2), waits_for(3));

    // Three in a circle.
    let given = input(vec![moved_here(synced_file(2, ROOT, "x"), ROOT, "y"), moved_here(synced_file(3, ROOT, "y"), ROOT, "z"), moved_here(synced_file(4, ROOT, "z"), ROOT, "x")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: temporary_name(2) }));
    assert_eq!((answer(&given, 3), answer(&given, 4)), (waits_for(4), waits_for(2)));
}

/// B2b: no rule today (each move finds the other's marked copy at its new path and is
/// skipped: both copies stay at the old names). New: through a temporary name here.
#[test]
fn items_onedrive_exchanged_the_names_of_go_through_a_temporary_name_in_the_folder() {
    let given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "x"), ROOT, "y"), moved_in_onedrive(synced_directory(3, ROOT, "y"), ROOT, "x")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveToTemporaryName { item: versions(&given, 2), place: place(ROOT, ".konedrive-swap-2") }));
    assert_eq!(answer(&given, 3), waits_for(2));

    // Once the folder shows the first under the temporary name, which is then the place
    // agreed on, the second is moved, and the first after it.
    let mut first = moved_in_onedrive(synced_file(2, ROOT, "x"), ROOT, "y");
    first.local.as_mut().unwrap().place = Some(place(ROOT, ".konedrive-swap-2"));
    first.synced_place = Some(place(ROOT, ".konedrive-swap-2"));
    let given = input(vec![first, moved_in_onedrive(synced_directory(3, ROOT, "y"), ROOT, "x")]);
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
    assert_eq!(answer(&given, 2), waits_for(3));
}

/// New: an item OneDrive moved that is in the way of another, and cannot go to its place
/// yet, goes through a temporary name too — in the directory OneDrive has it in where the
/// folder can bring it there, and where it is otherwise.
#[test]
fn an_item_onedrive_moved_that_is_in_the_way_of_another_goes_through_a_temporary_name_here() {
    // OneDrive made a directory at the name of one it moved into the new one.
    let given = input(vec![moved_in_onedrive(synced_directory(2, ROOT, "x"), id(3), "in"), listed(3, ItemKind::Directory, ROOT, "x")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveToTemporaryName { item: versions(&given, 2), place: place(ROOT, ".konedrive-swap-2") }));
    assert_eq!(answer(&given, 3), waits_for(2));

    // OneDrive moved 2 out of 3 to the name of 3, and 3 into 2: the folder brings 2 to its
    // directory, where the name is still taken.
    let given = input(vec![moved_in_onedrive(synced_directory(2, id(3), "z"), ROOT, "x"), moved_in_onedrive(synced_directory(3, ROOT, "x"), id(2), "X")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveToTemporaryName { item: versions(&given, 2), place: place(ROOT, ".konedrive-swap-2") }));
    assert_eq!(answer(&given, 3), waits_for(2));

    // OneDrive removed a directory and moved a file out of it to the directory's name.
    let given = input(vec![moved_in_onedrive(synced_file(2, id(3), "a"), ROOT, "z"), removed_in_onedrive(synced_directory(3, ROOT, "z"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveToTemporaryName { item: versions(&given, 2), place: place(ROOT, ".konedrive-swap-2") }));
    assert_eq!(answer(&given, 3), waits_for(2));

    // OneDrive removed a directory, made another at its name and moved a file from the one
    // into the other: the directory that goes is in the way, and steps aside where it is.
    let given = input(vec![listed(2, ItemKind::Directory, ROOT, "d"), removed_in_onedrive(synced_directory(3, ROOT, "d")), moved_in_onedrive(synced_file(4, id(3), "a.txt"), id(2), "a.txt")]);
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveToTemporaryName { item: versions(&given, 3), place: place(ROOT, ".konedrive-swap-3") }));
    assert_eq!((answer(&given, 2), answer(&given, 4)), (waits_for(3), waits_for(2)));

    // An item that is under its temporary name there already goes through no other: what a
    // temporary name did not break is a fault the plan says.
    let mut at_temporary_name = moved_in_onedrive(synced_directory(2, ROOT, ".konedrive-swap-2"), id(3), "in");
    at_temporary_name.onedrive_etag = Some("e".to_owned());
    let given = input(vec![at_temporary_name, listed(3, ItemKind::Directory, ROOT, ".konedrive-swap-2")]);
    assert_eq!(answer(&given, 2), Answer::OtherFault(PlanFault::WaitWithoutEnd(id(2))));
}

/// New (today: 409 and then a temporary name, as B2a): a directory made at the name of an
/// item that was moved into it. The item waits for the directory to be in OneDrive, and
/// the directory for its name.
#[test]
fn a_directory_made_at_the_name_of_an_item_moved_into_it_is_made_under_a_temporary_name() {
    let given = input(vec![moved_here(synced_file(2, ROOT, "a"), id(3), "a.txt"), made_directory(3, ROOT, "a")]);
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 3), parent: reference("root"), name: temporary_name(3) }));
    assert_eq!(answer(&given, 2), waits_for(3));
}

/// B5: differs from today, which foresees nothing: the second is sent, answered 409,
/// renamed to a copy's name and sent (`upload/steps/shared.rs`, untested for two local
/// items), its names compared by lower case alone.
#[test]
fn of_two_items_of_the_folder_with_one_name_for_onedrive_the_later_is_renamed_to_a_conflict_copys_name() {
    let given = input(vec![made_file(2, ROOT, "a.txt"), made_file(3, ROOT, "A.txt"), synced_file(4, ROOT, "b.txt"), made_directory(5, ROOT, "B.txt"), made_file(7, ROOT, "É"), moved_here(synced_file(6, ROOT, "was"), ROOT, "e\u{301}")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendNewFile { item: versions(&given, 2), parent: reference("root"), name: "a.txt".into() }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 3), name: "A-pc.txt".into() }));
    assert_eq!(answer(&given, 4), Answer::Nothing);
    assert_eq!(answer(&given, 5), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 5), name: "B-pc.txt".into() }), "OneDrive has the other under the name");
    // By the stamp, not by the id: the item 6 came to be unsent before the item 7.
    assert_eq!(answer(&given, 6), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 6), parent: reference("root"), name: "e\u{301}".into() }));
    assert_eq!(answer(&given, 7), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 7), name: "É-pc".into() }), "a composed letter and its two parts are one name");
}

/// B6a: differs from today, where the outcome depends on which is seen first (the worker
/// adopts, `upload/steps/meta.rs`; the cycle moves the local one aside,
/// `fuse/cycle_step/cycle_actions.rs`). One rule now.
#[test]
fn a_new_directory_here_and_a_new_one_in_onedrive_of_one_name_are_merged() {
    for name_here in ["d", "D"] {
        let given = input(vec![made_directory(2, ROOT, name_here), listed(3, ItemKind::Directory, ROOT, "d"), made_file(4, id(2), "mine.txt")]);
        assert_eq!(answer(&given, 2), row_step(RowStep::MergeItems { local: versions(&given, 2), onedrive: versions(&given, 3) }), "{name_here}");
        assert_eq!(answer(&given, 3), waits_for(2), "{name_here}");
        assert_eq!(answer(&given, 4), waits_for(2), "{name_here}");
    }
}

/// B6b and B6c. Today: `a_new_file_whose_name_is_taken_by_the_same_content_is_adopted` and
/// its other half (`upload/tests/conflicts.rs`), when the worker comes first; when the
/// cycle does, a conflict copy with no hash compared
/// (`an_item_that_appears_where_the_users_own_is_moves_it_aside`,
/// `fuse/cycle_step/cycle_actions/tests.rs`). One rule now, and the hash is a step.
#[test]
fn a_new_file_here_and_a_new_one_in_onedrive_of_one_name_are_merged_when_their_hash_is_one() {
    // The content of `listed(3, …)` is of the hash `h3`.
    let plan_for = |content: ContentVersion| {
        let given = input(vec![with_content_here(made_file(2, ROOT, "a.txt"), content), listed(3, ItemKind::File, ROOT, "a.txt")]);
        (answer(&given, 2), answer(&given, 3), versions(&given, 2), versions(&given, 3))
    };
    let (local, onedrive, local_versions, onedrive_versions) = plan_for(hashed_here("h3"));
    assert_eq!(local, row_step(RowStep::MergeItems { local: local_versions, onedrive: onedrive_versions }));
    assert_eq!(onedrive, waits_for(2));
    let (local, onedrive, local_versions, _) = plan_for(hashed_here("mine"));
    assert_eq!(local, folder_step(FolderStep::RenameToConflictCopy { item: local_versions, name: "a-pc.txt".into() }));
    assert_eq!(onedrive, waits_for(2));
    let (local, onedrive, local_versions, _) = plan_for(written_here());
    assert_eq!(local, row_step(RowStep::HashLocalContent { item: local_versions }), "of one size, and no hash known here");
    assert_eq!(onedrive, waits_for(2));
}

/// New: the design's "a conflict copy's name is chosen free on both sides, as OneDrive
/// compares names". Today the name is checked against the disk alone (`pick_copy_name`,
/// `upload/local.rs`), and a 409 makes a copy of the copy.
#[test]
fn a_conflict_copys_name_is_free_on_both_sides_as_onedrive_compares_names() {
    let given = input(vec![
        with_content_here(made_file(2, ROOT, "a.txt"), hashed_here("mine")),
        listed(3, ItemKind::File, ROOT, "a.txt"),
        made_file(4, ROOT, "A-PC.txt"),
        listed(5, ItemKind::File, ROOT, "a-pc-2.TXT"),
        made_file(6, id(8), "a-pc-3.txt"),
        synced_directory(8, ROOT, "elsewhere"),
    ]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 2), name: "a-pc-3.txt".into() }));

    // Two items that step aside in one plan do not take one name.
    let given = input(vec![synced_file(2, ROOT, "a.txt"), made_file(3, ROOT, "A.txt"), made_file(4, ROOT, "a.TXT")]);
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 3), name: "A-pc.txt".into() }));
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 4), name: "a-pc-2.TXT".into() }));
}

/// Open: an item that is left here (its name is ignored) at the name an item of OneDrive
/// comes to. The design has no case.
///
/// - Reading 1 (implemented; today's, `a_local_only_entry_in_the_way_is_moved_aside`,
///   `fuse/conflict_copies/tests.rs`): the item that is left here is renamed to a conflict
///   copy's name, and OneDrive's appears. It is never merged with OneDrive's: an ignored
///   directory that became one with a directory of OneDrive would have all in it sent.
/// - Reading 2: OneDrive's item is not shown while the other has its name: it waits for
///   the user.
#[test]
fn open_an_item_that_is_left_here_at_the_name_of_an_item_of_onedrive_is_renamed_to_a_conflict_copys_name() {
    let mut given = input(vec![made_directory(2, ROOT, "build"), listed(3, ItemKind::Directory, ROOT, "build"), made_file(4, ROOT, "x.tmp"), moved_in_onedrive(synced_file(5, ROOT, "was"), ROOT, "x.tmp")]);
    given.ignored_items = [id(2), id(4)].into();
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 2), name: "build-pc".into() }));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 4), name: "x-pc.tmp".into() }));
    assert_eq!(answer(&given, 5), waits_for(4));
}

/// The map's "a local-only entry moved to an ignored name leaves the queue; one already
/// going up is deleted afterwards" (`a_fresh_file_renamed_to_an_ignored_name_while_it_is_sent_is_removed_from_onedrive`,
/// `fuse/detections/entry_renamed/tests.rs`). The folder goes on showing the item: the
/// step's commit gives its binding up and confirms the removal of the row that took it.
///
/// Open, twice.
///
/// What the row can say. The plan reads "got its binding while it was sent, and its name
/// is ignored now" as: the row has a binding, both sides have the item, and
/// `is_name_ignored` is set. That is the case only if the flag is written as the design's
/// §3 says, "where an item without a binding gets a name". `ItemStore` writes it with every
/// local place (`LocalVersionChange::Moved`), also of an item OneDrive has, and the
/// design's §4 names "a step towards the folder" among its writers: a row written so says
/// the same of an item of OneDrive that was renamed here to an ignored name, or shown
/// under one, and its removal would be sent.
///
/// - Reading 1 (implemented, the design's §3): the flag on a row that has a binding is
///   written by nothing but the race; the removal is sent.
/// - Reading 2: the rows cannot tell the race from a rename, and the plan sends no removal
///   on the flag; the step whose sending was running decides at its commit, or the plan is
///   given the fact.
///
/// A directory OneDrive has something in. The design says "its removal is sent", which
/// would remove from OneDrive what was sent into it since.
///
/// - Reading 1 (implemented): such a directory stays as it is, in OneDrive too.
/// - Reading 2: its removal is sent with all OneDrive has in it.
#[test]
fn open_an_item_that_got_its_binding_while_it_was_sent_and_whose_name_is_ignored_now_has_its_removal_sent() {
    let ignored = |mut item: Item| {
        item.is_name_ignored = true;
        item
    };
    let given = input(vec![ignored(on_disk(synced_file(2, ROOT, "notes.tmp"))), ignored(synced_directory(3, ROOT, "build")), ignored(synced_directory(4, ROOT, "target")), synced_file(5, id(4), "sent.o")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 3), below: Vec::new() }));
    assert_eq!(answer(&given, 4), Answer::Nothing, "open: OneDrive has something in it");
    assert_eq!(answer(&given, 5), Answer::Nothing);
}

// -- directories

/// B3: no rule today (Graph's answer to such a move is not measured). The design: OneDrive's
/// move stands and the local one is taken back.
#[test]
fn a_directory_moved_here_into_one_that_onedrive_moved_into_it_is_taken_back() {
    let given = input(vec![moved_here(synced_directory(2, ROOT, "a"), id(3), "a"), moved_in_onedrive(synced_directory(3, ROOT, "b"), id(2), "b")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeMoveBack { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), waits_for(2), "it cannot be moved here into what the folder shows in it");

    // Deeper: here 2 was moved into 4, which is in 3; OneDrive moved 3 into 2.
    let given = input(vec![moved_here(synced_directory(2, ROOT, "a"), id(4), "a"), moved_in_onedrive(synced_directory(3, ROOT, "b"), id(2), "b"), synced_directory(4, id(3), "c")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::TakeMoveBack { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), waits_for(2));
}

/// New: "a directory before what is in it", for two moves made here. A directory moved here
/// into one that OneDrive still has in it closes no cycle when that one was moved out of it
/// here; its move is sent after the other's.
#[test]
fn a_directory_moved_here_into_one_onedrive_still_has_in_it_is_sent_after_that_one_left_it() {
    let given = input(vec![moved_here(synced_directory(2, id(3), "in"), ROOT, "out"), moved_here(synced_directory(3, ROOT, "d"), id(2), "d")]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "out".into() }));
    assert_eq!(answer(&given, 3), waits_for(2));
}

/// New: the name a directory comes back to when its move is taken back. OneDrive has the
/// directory under that name, and it stays there, so the user's own item at that name here
/// steps aside; it does not wait for a move that is never sent.
#[test]
fn the_users_own_at_the_name_a_move_is_taken_back_to_steps_aside() {
    let given = input(vec![moved_in_onedrive(synced_directory(2, ROOT, "a"), id(3), "a"), moved_here(synced_directory(3, ROOT, "b"), id(2), "b"), made_file(4, ROOT, "b")]);
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 4), name: "b-pc".into() }));
    assert_eq!(answer(&given, 3), waits_for(4), "its move is taken back once the name is free here");
    assert_eq!(answer(&given, 2), waits_for(3));
}

/// B7: differs from today, where a removal is sent for each entry, deepest first
/// (`a_removed_directory_tree_is_removed_entry_by_entry`, `upload/tests/row_kinds.rs`).
#[test]
fn a_directory_removed_here_with_all_in_it_is_one_removal_in_onedrive() {
    let given = input(vec![
        removed_here(synced_directory(2, ROOT, "d")),
        removed_here(synced_file(3, id(2), "a.txt")),
        removed_here(synced_directory(4, id(2), "e")),
        removed_here(synced_file(5, id(4), "b.txt")),
    ]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 3), versions(&given, 4), versions(&given, 5)] }));
    for below in [3, 4, 5] {
        assert_eq!(answer(&given, below), waits_for(2), "{below}");
    }
}

/// The map's rule beside B8 (`a_folders_removal_waits_for_what_is_moved_out_of_it`,
/// `outbox/pick/tests.rs`): a directory's removal waits for every unsent change of what
/// OneDrive has below it, so that an item moved out of it is not removed with it.
#[test]
fn the_removal_of_a_directory_waits_for_what_was_moved_out_of_it() {
    let given = input(vec![removed_here(synced_directory(2, ROOT, "d")), removed_here(synced_directory(3, id(2), "e")), moved_here(synced_file(4, id(3), "kept.txt"), ROOT, "kept.txt")]);
    assert_eq!(answer(&given, 2), waits_for(4));
    assert_eq!(answer(&given, 3), waits_for(2), "one removal, the outermost directory's");
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "kept.txt".into() }));
}

/// B4c. Today: `a_removed_directory_is_removed_whole_and_unguarded`
/// (`upload/tests/row_kinds.rs`): the removal is sent and stands, whatever OneDrive changed
/// below it.
#[test]
fn the_removal_of_a_directory_stands_whatever_onedrive_changed_in_it() {
    let given = input(vec![
        removed_here(synced_directory(2, ROOT, "d")),
        listed(3, ItemKind::File, id(2), "new.txt"),
        removed_here(with_content_in_onedrive(synced_file(4, id(2), "a.txt"), hashed("theirs"))),
    ]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 4)] }));
    assert_eq!(answer(&given, 3), waits_for(2), "it does not appear here");
    assert_eq!(answer(&given, 4), waits_for(2), "it does not come back");
}

/// Open: an item the folder shows that OneDrive moved into a directory that was removed
/// here. The design says the removal "is sent and stands" whatever OneDrive changed below
/// the directory, and that the folder follows OneDrive's move; it cannot do both.
///
/// - Reading 1 (implemented): the folder cannot follow, so the item's place here stands
///   and is sent, as where OneDrive's place cannot be taken (the map's P10); the removal
///   waits for it, as for an item moved out of the directory here. Nothing the folder
///   shows is removed by a removal of something else.
/// - Reading 2: the item goes with the removal, in OneDrive and then here.
#[test]
fn open_an_item_onedrive_moved_into_a_directory_removed_here_keeps_its_place_here() {
    let moved_on_both_sides = moved_in_onedrive(moved_here(synced_file(4, ROOT, "b.txt"), ROOT, "c.txt"), id(5), "b.txt");
    let given = input(vec![
        removed_here(synced_directory(2, ROOT, "d")),
        moved_in_onedrive(synced_file(3, ROOT, "a.txt"), id(2), "a.txt"),
        moved_on_both_sides,
        listed(5, ItemKind::Directory, id(2), "new"),
    ]);
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 3), parent: reference("root"), name: "a.txt".into() }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "c.txt".into() }), "in a directory OneDrive made in the removed one");
    assert_eq!(answer(&given, 2), waits_for(3));
    assert_eq!(answer(&given, 5), waits_for(2));
}

/// B4a and B4b, and the brief's §5, 4. Today:
/// `a_removed_directory_that_holds_a_local_only_entry_stays_and_is_made_again`
/// (`fuse/conflict_copies/tests.rs`); B4b has no test of the whole. Stage A left the
/// directory and the user's own item in it waiting for each other.
#[test]
fn a_directory_onedrive_removed_that_holds_something_of_the_users_own_stays_and_is_sent_again() {
    // An item OneDrive never had.
    let given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d")), made_file(3, id(2), "mine.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), waits_for(2));

    // Content changed here: the file gives up its binding first (the map's E7), and is then
    // an item OneDrive never had.
    let given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d")), removed_in_onedrive(with_content_here(synced_file(3, id(2), "a.txt"), written_here()))]);
    assert_eq!(answer(&given, 2), waits_for(3));
    assert_eq!(answer(&given, 3), row_step(RowStep::GiveUpBinding { item: versions(&given, 3) }));

    // An item that is left here, and one that was moved into the directory here.
    let mut given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d")), made_file(3, id(2), "x.tmp"), moved_here(synced_file(4, ROOT, "a.txt"), id(2), "a.txt")]);
    given.ignored_items = [id(3)].into();
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::NameIgnored));
    assert_eq!(answer(&given, 4), waits_for(2));

    // Two deep: the inner directory first, and the outer one once the inner is the user's own.
    let given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d")), removed_in_onedrive(synced_directory(3, id(2), "e")), made_file(4, id(3), "mine.txt")]);
    assert_eq!((answer(&given, 2), answer(&given, 4)), (waits_for(3), waits_for(3)));
    assert_eq!(answer(&given, 3), row_step(RowStep::GiveUpBinding { item: versions(&given, 3) }));
}

/// The design: "the clean items in it go first". A directory that gave up its binding
/// while the folder still showed a clean file in it would have that file read as moved
/// here, and sent again.
#[test]
fn the_clean_items_of_a_directory_onedrive_removed_go_before_it_gives_up_its_binding() {
    let given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d")), made_file(3, id(2), "mine.txt"), removed_in_onedrive(on_disk(synced_file(4, id(2), "clean.txt")))]);
    assert_eq!(answer(&given, 2), waits_for(4));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), folder_step(FolderStep::RemoveItem { item: versions(&given, 4) }));
}

/// New: which items stay in a directory OneDrive does not have, when it gives up its
/// binding, is read off their rows: one the two sides agreed on in it goes first, unless
/// its place here is to be sent into the directory again.
#[test]
fn a_directory_gives_up_its_binding_once_nothing_in_it_was_agreed_on_there() {
    // A directory no listing told of (the map's E-rows have none; stage A's open case 1).
    let never_listed = |number: i64, name: &str| {
        let mut directory = made_directory(number, ROOT, name);
        directory.onedrive_item = Some(reference(&format!("I{number}")));
        directory
    };
    let given = input(vec![never_listed(2, "d"), removed_in_onedrive(synced_file(3, id(2), "clean.txt")), made_file(4, id(2), "mine.txt")]);
    assert_eq!(answer(&given, 2), waits_for(3));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RemoveItem { item: versions(&given, 3) }));
    let given = input(vec![never_listed(2, "d"), made_file(4, id(2), "mine.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));

    // OneDrive removed the directory 3 and moved the file 4 out of it into the directory 2,
    // which was removed here: the file's place here stands, it is sent into 3 again, and 3
    // is made again for it.
    let given = input(vec![removed_here(synced_directory(2, ROOT, "gone")), removed_in_onedrive(synced_directory(3, ROOT, "d")), moved_in_onedrive(synced_file(4, id(3), "a.txt"), id(2), "a.txt")]);
    assert_eq!(answer(&given, 3), row_step(RowStep::GiveUpBinding { item: versions(&given, 3) }));
    assert_eq!((answer(&given, 4), answer(&given, 2)), (waits_for(3), waits_for(4)));
}

/// Open: an item OneDrive has that was moved here into a directory that is left here. The
/// design has no case; the directory is never in OneDrive.
///
/// - Reading 1 (implemented): the move waits for the user, as for a directory that is not
///   in OneDrive (`Reason::Parent`).
/// - Reading 2: the item leaves OneDrive (its removal is sent) and is left here with the
///   directory.
#[test]
fn open_an_item_moved_here_into_a_directory_that_is_left_here_waits_for_the_user() {
    let mut given = input(vec![made_directory(2, ROOT, "build"), moved_here(synced_file(3, ROOT, "a.txt"), id(2), "a.txt")]);
    given.ignored_items = [id(2)].into();
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::NameIgnored));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::Parent)));
}

/// New: a name of OneDrive is asked whether it can be a directory entry before the folder
/// takes it, and so is every directory above an item (`placement` does not say it).
#[test]
fn an_item_of_onedrive_in_a_directory_whose_name_the_folder_cannot_hold_is_not_shown() {
    let given = input(vec![listed(2, ItemKind::Directory, ROOT, "a/b"), listed(3, ItemKind::File, id(2), "in.txt")]);
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::CannotBeShown));
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::CannotBeShown));
}

// -- the guard against a mass removal

/// Differs from today, as the design says: today the guard runs as each removal is
/// recorded, and the first ones leave before it trips
/// (`a_mass_delete_is_held_until_confirmed`, `local/record/tests.rs`). The plan counts the
/// removals of everything it was given, a removed directory with all OneDrive has in it,
/// and none leaves before the user confirms.
#[test]
fn a_mass_removal_waits_for_the_user_to_confirm_it() {
    let removed: Vec<Item> = (2..12).map(|number| removed_here(synced_file(number, ROOT, &format!("f{number}")))).collect();
    let held = Answer::Wait(WaitReason::User(Reason::MassDelete));
    let mut given = input(removed.clone());
    given.shown_item_count = 40;
    for number in 2..12 {
        assert_eq!(answer(&given, number), held, "ten of forty: {number}");
    }
    given.shown_item_count = 50;
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 2) }), "a fifth, and no more");

    // What the user confirmed is sent and counted no more: nine are left, and are no mass
    // removal.
    given.shown_item_count = 40;
    given.items_with_confirmed_removal = [id(2)].into();
    for number in 2..12 {
        assert_eq!(answer(&given, number), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, number) }), "{number}");
    }

    // A removed directory counts with all OneDrive has in it, removed here or not.
    let mut tree = vec![removed_here(synced_directory(2, ROOT, "d"))];
    tree.extend((3..8).map(|number| removed_here(synced_file(number, id(2), &format!("f{number}")))));
    tree.extend((8..12).map(|number| listed(number, ItemKind::File, id(2), &format!("f{number}"))));
    let mut given = input(tree);
    given.shown_item_count = 40;
    assert_eq!(answer(&given, 2), held);
    assert_eq!(answer(&given, 3), waits_for(2));
    given.items_with_confirmed_removal = [id(2)].into();
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: (3..8).map(|number| versions(&given, number)).collect() }));
}

// -- the answer

/// The design's "for an item it needs and was not given, the function answers 'waits for
/// that item', and the caller widens the set".
#[test]
fn an_item_that_waits_for_an_item_that_was_not_given_says_which() {
    let given = input(vec![made_file(2, id(9), "a.txt"), listed(3, ItemKind::File, id(8), "b.txt"), synced_directory(4, ROOT, "d"), made_file(5, id(4), "c.txt")]);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::ItemNotGiven(id(9))));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::ItemNotGiven(id(8))));
    assert_eq!(answer(&given, 5), onedrive_step(OneDriveStep::SendNewFile { item: versions(&given, 5), parent: reference("I4"), name: "c.txt".into() }));
}

/// The brief: the next steps that can run now, oldest `unsent_since` first. What OneDrive
/// changed has no stamp, and goes after what is unsent, in the order it was given.
#[test]
fn the_steps_are_in_the_order_the_items_came_to_be_unsent() {
    let mut late = made_file(5, ROOT, "five");
    late.unsent_since = Some(9);
    let mut early = made_directory(4, ROOT, "four");
    early.unsent_since = Some(2);
    let given = input(vec![late, listed(7, ItemKind::File, ROOT, "seven"), listed(3, ItemKind::File, ROOT, "three"), early]);
    let plan = make_sync_plan(&given);
    assert_eq!(plan.steps.iter().map(SyncStep::local_item_id).collect::<Vec<_>>(), [id(4), id(5), id(7), id(3)]);
}

/// The invariant of today's `outbox/pick.rs` ("nothing runnable says why";
/// `a_stall_is_reported`, `outbox/pick/tests.rs`), carried over: an item that waits for
/// another item, where nothing the plan answers ends the wait, is a fault the plan says.
#[test]
fn a_wait_that_nothing_ends_is_a_fault_the_plan_says() {
    // Two rows say one name on the OneDrive side, and no listing is under way: the new one
    // waits for an item that has nothing to do.
    let mut given = input(vec![synced_file(2, ROOT, "a.txt"), listed(3, ItemKind::File, ROOT, "a.txt")]);
    assert_eq!(answer(&given, 3), Answer::OtherFault(PlanFault::WaitWithoutEnd(id(3))));
    // While a listing is being written the rows may say so: the second waits for it.
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing));
}

/// The brief's §5, 6: what cannot be judged while no listing is under way is a fault of
/// the rows that a full listing mends, not a wait.
#[test]
fn an_item_that_cannot_be_judged_while_no_listing_is_under_way_needs_a_full_listing() {
    let mut given = input(vec![listed(2, ItemKind::File, ROOT, "a.txt"), moved_in_onedrive(synced_file(3, ROOT, "b.txt"), ROOT, "c.txt"), made_file(4, ROOT, "new.txt")]);
    given.folder_visibility.clear();
    // No listing bound the root of the folder to the root of the drive.
    given.items[0].onedrive_item = None;
    given.items[0].onedrive = None;
    for number in [2, 3, 4] {
        assert_eq!(answer(&given, number), Answer::OtherFault(PlanFault::FullListingNeeded(id(number))), "{number}");
    }
    given.is_listing_under_way = true;
    for number in [2, 3, 4] {
        assert_eq!(answer(&given, number), Answer::Wait(WaitReason::Listing), "{number}");
    }
}

/// The invariant, over every set of rows of the enumeration: when no step can run, some
/// item waits for a time, for the user, for a step that is running or for a listing, or
/// the plan says it is at fault — or there is nothing to do.
#[test]
fn a_plan_in_which_nothing_can_run_says_why() {
    for (words, given) in enumeration::every_enumerated_input() {
        let plan = make_sync_plan(&given);
        if !plan.steps.is_empty() || !plan.faults.is_empty() {
            continue;
        }
        let ends = plan.waits.iter().any(|wait| matches!(wait.reason, WaitReason::Time { .. } | WaitReason::User(_) | WaitReason::RunningStep | WaitReason::Listing | WaitReason::ItemNotGiven(_)));
        assert!(ends || plan.waits.is_empty(), "{words}: {:?}", plan.waits);
    }
}
