//! What the plan decides between items: a test per case of the design's list
//! (`docs/tasks/item-versions-design.md`, §6, "what it decides between items"), the
//! closed set, the order of the steps and the rule that a plan
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

/// B1a: a taker waits for every freer of its name, as OneDrive compares names.
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

/// Found by the convergence run in which OneDrive is a state of its own (seed 82): an item
/// that is left here steps aside only for an item of OneDrive that needs its place — the
/// same name, byte for byte. It is never sent, so how OneDrive compares names has nothing
/// to say of it: beside an item OneDrive has under a name that only folds to its own, the
/// folder shows both, and it stays as it is. (A plan given the two rows renamed it; a plan
/// given what differs was never given it: the two did not agree.)
#[test]
fn an_item_that_is_left_here_stays_beside_an_item_of_onedrive_whose_name_only_folds_to_its_own() {
    let mut given = input(vec![synced_directory(2, ROOT, "~X.TMP"), made_file(3, ROOT, "~x.tmp"), made_file(4, ROOT, "Build"), listed(5, ItemKind::File, ROOT, "build")]);
    given.ignored_items = [id(3), id(4)].into();
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::NameIgnored));
    assert_eq!(answer(&given, 4), Answer::Exclusion(ExclusionReason::NameIgnored));
    assert_eq!(answer(&given, 5), folder_step(FolderStep::ShowItem { item: versions(&given, 5) }), "the folder holds the two names apart");
}

/// Decided after stage B (the brief's §6, 1; the design's §6): no removal is ever sent
/// because a name is ignored. An item OneDrive has is never left here, whatever its row says
/// of its name: one that got its binding while its sending was running and whose name is
/// ignored now stays in OneDrive under that name, and one renamed here to an ignored name
/// has its move sent like any other. Differs from today, where nothing of an ignored name
/// is recorded (`mount/upload_rules.rs`).
#[test]
fn a_bound_item_whose_name_is_ignored_is_planned_as_any_bound_item() {
    let ignored = |mut item: Item| {
        item.is_name_ignored = true;
        item
    };
    let given = input(vec![
        ignored(on_disk(synced_file(2, ROOT, "notes.tmp"))),
        ignored(synced_directory(3, ROOT, "build")),
        ignored(moved_here(synced_file(4, ROOT, "notes.txt"), ROOT, "notes.bak")),
        ignored(with_content_here(synced_file(5, ROOT, "draft.tmp"), written_here())),
        ignored(removed_in_onedrive(synced_file(6, ROOT, "gone.tmp"))),
    ]);
    assert_eq!(answer(&given, 2), Answer::Nothing);
    assert_eq!(answer(&given, 3), Answer::Nothing);
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "notes.bak".into() }));
    assert_eq!(answer(&given, 5), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 5) }));
    assert_eq!(answer(&given, 6), folder_step(FolderStep::RemoveItem { item: versions(&given, 6) }));
}

/// The design's "OneDrive is the judge" (§6, "Names"; §7, what a step remembers): new. The
/// plan's own rule says two names are two, OneDrive answered the sending that the name is
/// taken, and that answer is given: the item steps aside as from any name another item
/// holds for good. Today the worker makes the copy on the spot (`upload/steps/shared.rs`).
#[test]
fn an_item_whose_name_onedrive_answered_is_taken_is_renamed_to_a_conflict_copys_name() {
    let mut given = input(vec![made_file(2, ROOT, "Straße.txt"), moved_here(synced_file(3, ROOT, "b.txt"), ROOT, "c.txt"), made_directory(4, ROOT, "d")]);
    given.places_taken_in_onedrive = [(id(2), place(ROOT, "Straße.txt")), (id(3), place(ROOT, "c.txt")), (id(4), place(ROOT, "elsewhere"))].into();
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 2), name: "Straße-pc.txt".into() }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 3), name: "c-pc.txt".into() }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 4), parent: reference("root"), name: "d".into() }), "the answer was of another place");

    // A listing under way may yet bring the item that has the name.
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
}

/// The design's §3: "while [a listing] is being written, page by page, two rows may say one
/// name on the OneDrive side, and the plan then lets the second wait". Found by the
/// convergence test (a listing of two pages, the first with half of an exchange of two
/// names): both items were brought to the one place here in one plan. New.
#[test]
fn of_two_items_the_rows_say_onedrive_has_under_one_name_the_second_waits_for_the_listing() {
    let mut given = input(vec![moved_in_onedrive(synced_file(2, ROOT, "a.txt"), ROOT, "d"), moved_in_onedrive(synced_file(3, ROOT, "b.txt"), ROOT, "D"), listed(4, ItemKind::File, ROOT, "d")]);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing), "one name, as OneDrive compares names");
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::Listing));

    // With no listing under way OneDrive has judged of the two names that fold to one:
    // they are two items (the brief's §7, M3). The very name on two rows is what OneDrive
    // cannot have: a full listing mends the rows.
    given.is_listing_under_way = false;
    assert_eq!(answer(&given, 2), folder_step(FolderStep::MoveItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
    assert_eq!(answer(&given, 4), Answer::OtherFault(PlanFault::FullListingNeeded(id(4))));
}

// -- directories

/// Found by the convergence test: the steps of one plan run at once, and a directory that
/// gives up its binding takes what the two sides agreed is in it, and what OneDrive has in
/// it, to the row that takes the binding — the rows of those items change under their own
/// steps. So an item whose place agreed on is in such a directory, and that the folder
/// shows elsewhere or not at all, waits for the directory. New.
#[test]
fn what_was_agreed_to_be_in_a_directory_that_gives_up_its_binding_waits_for_it() {
    let given = input(vec![
        removed_in_onedrive(synced_directory(2, ROOT, "d")),
        made_file(3, id(2), "mine.txt"),
        removed_in_onedrive(on_disk(moved_here(synced_file(4, id(2), "a.txt"), ROOT, "a.txt"))),
        removed_in_onedrive(removed_here(synced_file(5, id(2), "b.txt"))),
        removed_in_onedrive(moved_here(synced_file(6, id(2), "c.txt"), ROOT, "c.txt")),
    ]);
    assert_eq!(answer(&given, 2), row_step(RowStep::GiveUpBinding { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 4), waits_for(2), "it gives up its own binding after");
    assert_eq!(answer(&given, 5), waits_for(2), "its row is dropped after");
    assert_eq!(answer(&given, 6), waits_for(2), "it leaves the folder after");
}

/// Found by the larger convergence run (seed 73, the caller's facts drawn): the steps of one
/// plan run in any order, and two moves towards the folder must not close a cycle between
/// them. `d` was moved here into `e`; OneDrive moved `e` into `c` and `c` into `d`. The move
/// here is taken back. Were `e` moved into `c` first, the folder would show `d` below `c`,
/// and refuse `c` its move into `d`: so `e` waits for `c`, whose move is the earlier one's.
#[test]
fn two_directories_onedrive_moved_are_not_both_moved_here_where_that_closes_a_cycle_in_the_folder() {
    let given = input(vec![moved_in_onedrive(synced_directory(3, ROOT, "c"), id(4), "c"), moved_here(synced_directory(4, ROOT, "d"), id(5), "d"), moved_in_onedrive(synced_directory(5, ROOT, "e"), id(3), "e")]);
    assert_eq!(answer(&given, 4), folder_step(FolderStep::TakeMoveBack { item: versions(&given, 4) }));
    assert_eq!(answer(&given, 3), folder_step(FolderStep::MoveItem { item: versions(&given, 3) }));
    assert_eq!(answer(&given, 5), waits_for(3));
}

/// Found by the convergence run in which OneDrive is a state of its own (seed 157): a new
/// directory here and a new one in OneDrive are one item, and the merge moves the binding
/// to the row of the folder's — with it, what OneDrive has in the directory is in that row.
/// A step of such an item would find its row changed beside the merge: it waits for it.
#[test]
fn what_onedrive_has_in_a_directory_that_is_merged_waits_for_the_merge() {
    let given = input(vec![made_directory(2, ROOT, "d"), listed(3, ItemKind::Directory, ROOT, "d"), moved_in_onedrive(removed_here(synced_file(4, ROOT, "b.txt")), id(3), "e.txt")]);
    assert_eq!(answer(&given, 2), row_step(RowStep::MergeItems { local: versions(&given, 2), onedrive: versions(&given, 3) }));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), waits_for(2), "its removal is sent once its row says where OneDrive has it");
}

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

/// The map's rule beside B8: a directory's removal waits for every unsent change of what
/// OneDrive has below it, so that an item moved out of it is not removed with it.
#[test]
fn the_removal_of_a_directory_waits_for_what_was_moved_out_of_it() {
    let given = input(vec![removed_here(synced_directory(2, ROOT, "d")), removed_here(synced_directory(3, id(2), "e")), moved_here(synced_file(4, id(3), "kept.txt"), ROOT, "kept.txt")]);
    assert_eq!(answer(&given, 2), waits_for(4));
    assert_eq!(answer(&given, 3), waits_for(2), "one removal, the outermost directory's");
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "kept.txt".into() }));
}

// B4c, "the removal of a directory stands whatever OneDrive changed in it", is in
// `directory_removal`, beside the rules that replaced it but for a rename or a move.

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

/// Found by the convergence test (the plan given two items at a time, and no directory
/// above them): the root is an item like any other to the closed set. An item the folder
/// shows in the root, whose row of the root was not given, waits for that row; it is not
/// a fault of the rows that a full listing would mend.
#[test]
fn an_item_in_the_root_whose_row_was_not_given_waits_for_the_root() {
    let mut given = input(vec![made_file(2, ROOT, "a.txt"), moved_here(synced_file(3, ROOT, "b.txt"), ROOT, "c.txt")]);
    given.items.retain(|item| item.local_item_id != ROOT);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::ItemNotGiven(ROOT)));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::ItemNotGiven(ROOT)));
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

/// The invariant "nothing runnable says why": an item that waits for
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
    enumeration::visit_every_enumerated_input(|words, given| {
        let plan = make_sync_plan(given);
        if !plan.steps.is_empty() || !plan.faults.is_empty() {
            return;
        }
        let ends = plan.waits.iter().any(|wait| matches!(wait.reason, WaitReason::Time { .. } | WaitReason::User(_) | WaitReason::RunningStep | WaitReason::StartUpCheck | WaitReason::Listing | WaitReason::ItemNotGiven(_)));
        assert!(ends || plan.waits.is_empty(), "{words}: {:?}", plan.waits);
    });
}

// -- the findings of the review of part 3

/// The second review's S1: between two items OneDrive lists, OneDrive has judged — also
/// where one of them is renamed here. Two files OneDrive has in one directory under names
/// that fold to one; one is renamed here within that fold, to the other's name in another
/// letter case, or only to another case of its own. Its move is sent: it has a name of
/// that fold in OneDrive already, beside the other. A refusal would be OneDrive's to say.
#[test]
fn an_item_renamed_within_the_names_onedrive_keeps_apart_is_sent() {
    for name in ["E\u{301}CLAIR", "\u{c9}CLAIR"] {
        let given = input(vec![moved_here(synced_file(2, ROOT, "\u{e9}clair"), ROOT, name), synced_file(3, ROOT, "e\u{301}clair")]);
        assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: name.into() }), "{name}");
        assert_eq!(answer(&given, 3), Answer::Nothing);
    }
    // While a listing is under way it may yet say one of the two is elsewhere.
    let mut given = input(vec![moved_here(synced_file(2, ROOT, "\u{e9}clair"), ROOT, "\u{c9}CLAIR"), synced_file(3, ROOT, "e\u{301}clair")]);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
    // An item that comes to the name from elsewhere steps aside, as before.
    let given = input(vec![moved_here(synced_file(2, ROOT, "other"), ROOT, "\u{c9}CLAIR"), synced_file(3, ROOT, "e\u{301}clair")]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 2), name: "\u{c9}CLAIR-pc".into() }));
}

/// The review's M1, new: a directory removed here that OneDrive has in a directory removed
/// here sends no removal of its own, whatever is answered for the outer one; what was
/// moved out of it is sent first, and the outer one's removal is the one removal.
#[test]
fn a_directory_removed_here_in_a_removed_directory_sends_no_removal_of_its_own() {
    let rows = || vec![
        removed_here(synced_directory(2, ROOT, "a")),
        removed_here(synced_directory(3, id(2), "b")),
        moved_here(synced_file(4, id(3), "c.txt"), ROOT, "kept.txt"),
        removed_here(synced_file(5, id(2), "c.txt")),
        removed_here(synced_file(6, id(2), "e.txt")),
    ];
    // A step is running for the outer directory: its move, sent before it was removed.
    let mut given = input(rows());
    given.items_with_running_step = [id(2)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::RunningStep));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "kept.txt".into() }));
    for inside in [3, 5, 6] {
        assert_eq!(answer(&given, inside), waits_for(2), "item {inside} goes with the outer directory");
    }
    // No step is running: the outer one waits for what was moved out of the inner one.
    let given = input(rows());
    assert_eq!(answer(&given, 2), waits_for(4));
    assert_eq!(answer(&given, 3), waits_for(2));
    // Once the move was sent, one removal, of the outer one, for all four.
    let mut sent = rows();
    sent[2] = synced_file(4, ROOT, "kept.txt");
    let given = input(sent);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 3), versions(&given, 5), versions(&given, 6)] }));
    assert_eq!(answer(&given, 3), waits_for(2));
}

/// M1's condition, new: a directory's removal is not sent while a step is running for
/// anything OneDrive has below it, at any depth, though the folder does not show it.
#[test]
fn the_removal_of_a_directory_waits_for_a_step_running_for_what_onedrive_has_below_it() {
    let mut given = input(vec![removed_here(synced_directory(2, ROOT, "a")), removed_here(synced_directory(3, id(2), "b")), removed_here(synced_file(4, id(3), "c.txt"))]);
    given.items_with_running_step = [id(4)].into();
    assert_eq!(answer(&given, 2), waits_for(4));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::RunningStep));
}

/// The review's S2, new: while a listing is under way the rows are behind OneDrive, and no
/// directory's removal is sent.
#[test]
fn the_removal_of_a_directory_waits_for_a_listing_under_way_to_end() {
    let mut given = input(vec![removed_here(synced_directory(2, ROOT, "d")), removed_here(synced_file(3, id(2), "f")), removed_here(synced_file(4, ROOT, "g"))]);
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Listing));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 4) }), "a file's removal is guarded by its eTag");
}

fn cannot_be_shown(mut given: SyncPlanInput, numbers: &[i64]) -> SyncPlanInput {
    given.folder_visibility.extend(numbers.iter().map(|number| (id(*number), FolderVisibility::CannotBeShown)));
    given
}

/// The review's M2, the brief's §7: a new item here at the name of an item of OneDrive the
/// folder cannot show is not merged with it; OneDrive holds the name, so the local one is
/// renamed to a conflict copy's name, and then sent as new.
#[test]
fn a_new_item_at_the_name_of_an_item_the_folder_cannot_show_is_renamed_to_a_conflict_copys_name() {
    let given = cannot_be_shown(input(vec![listed(2, ItemKind::Directory, ROOT, "shared"), made_directory(3, ROOT, "shared"), made_file(4, id(3), "mine.txt")]), &[2]);
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 3), name: "shared-pc".into() }));
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::CannotBeShown));
    // A file of the very content of the one the folder cannot show, likewise.
    let given = cannot_be_shown(input(vec![listed(2, ItemKind::File, ROOT, "a.txt"), with_content_here(made_file(3, ROOT, "a.txt"), hashed_here("h2"))]), &[2]);
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RenameToConflictCopy { item: versions(&given, 3), name: "a-pc.txt".into() }));
}

/// M2, the brief's §7: what the user made, or moved here, in a directory OneDrive has
/// where the folder cannot show it is not sent into it and waits for the user; and that
/// directory does not leave the folder while the folder shows in it something unsent. An
/// item both sides agreed on there, changed here, has its content sent where OneDrive has
/// it.
#[test]
fn what_the_user_made_in_a_directory_the_folder_cannot_show_waits_for_the_user_and_the_directory_stays() {
    let given = cannot_be_shown(
        input(vec![synced_directory(2, ROOT, "d"), synced_file(3, id(2), "f"), made_file(4, id(2), "mine.txt"), moved_here(synced_file(5, ROOT, "g"), id(2), "g"), with_content_here(synced_file(6, id(2), "h"), written_here())]),
        &[2, 3, 6],
    );
    assert_eq!(answer(&given, 3), folder_step(FolderStep::RemoveItem { item: versions(&given, 3) }), "the clean item goes");
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::User(Reason::Parent)));
    assert_eq!(answer(&given, 5), Answer::Wait(WaitReason::User(Reason::Parent)));
    assert_eq!(answer(&given, 6), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 6) }));
    assert_eq!(answer(&given, 2), waits_for(3));
    // With the clean item gone, the directory waits for what is unsent in it.
    let given = cannot_be_shown(input(vec![synced_directory(2, ROOT, "d"), made_file(4, id(2), "mine.txt")]), &[2]);
    assert_eq!(answer(&given, 2), waits_for(4));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::User(Reason::Parent)));
    // While a listing is under way a later page may say otherwise of the directory.
    let mut listing = given.clone();
    listing.is_listing_under_way = true;
    assert_eq!(answer(&listing, 4), Answer::Wait(WaitReason::Listing));
}

/// The review's M3, the brief's §7: two items OneDrive lists in one directory under names
/// that fold to one and are not the same bytes are two items once no listing is under
/// way, and both are shown, each under its own name. The same bytes on two rows stay a
/// fault that a full listing mends.
#[test]
fn two_items_onedrive_lists_under_names_that_fold_to_one_are_both_shown() {
    let mut given = input(vec![listed(2, ItemKind::File, ROOT, "\u{e9}clair"), listed(3, ItemKind::File, ROOT, "e\u{301}clair"), listed(4, ItemKind::File, ROOT, "A.txt"), listed(5, ItemKind::File, ROOT, "a.txt")]);
    for shown in [2, 3, 4, 5] {
        assert_eq!(answer(&given, shown), folder_step(FolderStep::ShowItem { item: versions(&given, shown) }));
    }
    given.is_listing_under_way = true;
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Listing), "a page to come may say the first was renamed");
    let given = input(vec![listed(2, ItemKind::File, ROOT, "a.txt"), listed(3, ItemKind::File, ROOT, "a.txt")]);
    assert_eq!(answer(&given, 3), Answer::OtherFault(PlanFault::FullListingNeeded(id(3))));
}

/// The review's S7, new: a directory whose answer is made from what is in it, given
/// without all of it, says so, and is not planned from what was given.
#[test]
fn a_directory_given_without_all_that_is_in_it_says_so() {
    // Removed here: all OneDrive has in it and below it.
    let mut given = input(vec![removed_here(synced_directory(2, ROOT, "d")), removed_here(synced_file(3, id(2), "f"))]);
    given.onedrive_item_count_below.insert(id(2), 2);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::ItemsInDirectoryNotGiven(id(2))));
    assert_eq!(answer(&given, 3), waits_for(2));
    given.onedrive_item_count_below.remove(&id(2));
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::ItemsInDirectoryNotGiven(id(2))), "no count is no closed set");
    // One that leaves the folder, OneDrive having removed it or the folder being unable to
    // show it, and one that gives up its binding: what the folder shows in it.
    let mut given = input(vec![removed_in_onedrive(synced_directory(2, ROOT, "d"))]);
    given.shown_item_count_in.insert(id(2), 1);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::ItemsInDirectoryNotGiven(id(2))));
    let mut given = cannot_be_shown(input(vec![synced_directory(2, ROOT, "d")]), &[2]);
    given.shown_item_count_in.insert(id(2), 1);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::ItemsInDirectoryNotGiven(id(2))));
    // A directory both sides have and agree on needs nothing of what is in it.
    let mut given = input(vec![synced_directory(2, ROOT, "d"), made_file(3, id(2), "new.txt")]);
    given.shown_item_count_in.clear();
    given.onedrive_item_count_below.clear();
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendNewFile { item: versions(&given, 3), parent: reference("I2"), name: "new.txt".into() }));
}

/// The review's S8 (stage C's 0.3, reading 2), new: when an item's place waits for the
/// user, its content is still decided by the content table.
#[test]
fn the_content_of_an_item_whose_place_waits_for_the_user_is_still_decided() {
    let left_here = || {
        let mut directory = made_directory(2, ROOT, "~left");
        directory.is_name_ignored = true;
        directory
    };
    // Moved here into a directory that is left here, and changed here: the content is sent.
    let mut given = input(vec![left_here(), with_content_here(moved_here(synced_file(3, ROOT, "a.txt"), id(2), "a.txt"), written_here())]);
    given.ignored_items = [id(2)].into();
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 3) }));
    // Changed in OneDrive: the file takes OneDrive's content.
    let mut given = input(vec![left_here(), with_content_in_onedrive(moved_here(synced_file(3, ROOT, "a.txt"), id(2), "a.txt"), hashed("theirs"))]);
    given.ignored_items = [id(2)].into();
    assert_eq!(answer(&given, 3), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 3) }));
    // Nothing of its content differs: it waits for the user, with the reason of its place.
    let mut given = input(vec![left_here(), moved_here(synced_file(3, ROOT, "a.txt"), id(2), "a.txt")]);
    given.ignored_items = [id(2)].into();
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::Parent)));
    // The wait for the user is that of the directory it was moved into: a name OneDrive
    // cannot hold.
    let mut given = input(vec![made_directory(2, ROOT, "a:b"), with_content_here(moved_here(synced_file(3, ROOT, "a.txt"), id(2), "a.txt"), written_here())]);
    given.name_refusals.insert(id(2), Reason::NameCharacters);
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::NameCharacters)));
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 3) }));
    // A file a program holds open for writing is not planned, its content neither.
    given.items_open_for_writing = [id(3)].into();
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));
}

/// The class behind M1 (the review's S9), new: a new item here is not merged with a new
/// item of OneDrive a step is running for; it waits for that step.
#[test]
fn a_new_item_is_not_merged_with_an_item_a_step_is_running_for() {
    let mut given = input(vec![made_directory(2, ROOT, "d"), listed(3, ItemKind::Directory, ROOT, "d")]);
    given.items_with_running_step = [id(3)].into();
    assert_eq!(answer(&given, 2), waits_for(3));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::RunningStep));
}

// -- what the steps remember, and what the check at start left

/// New, from the review of the callers: what the steps remember of an item is told to the
/// plan, so that an item held by it answers a wait, and what is behind it waits for it. An
/// item whose next try is later waits until then, with the reason of its last failure, in
/// place of any step towards OneDrive; once that time has come it is sent.
#[test]
fn an_item_whose_next_try_is_later_waits_until_then() {
    let mut given = input(vec![made_directory(2, ROOT, "d"), made_file(3, id(2), "a.txt"), removed_here(synced_file(4, ROOT, "b.txt")), with_content_in_onedrive(synced_file(5, ROOT, "c.txt"), hashed("theirs"))]);
    let later = |reason: Reason| NextTry { not_before: given.now + 60, reason };
    given.next_tries = [(id(2), later(Reason::Network)), (id(4), later(Reason::WaitingForSpace)), (id(5), later(Reason::Network))].map(|(item, next_try)| ((item, TriedStep::TowardsOneDrive), next_try)).into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Time { until: given.now + 60, reason: Reason::Network }));
    assert_eq!(answer(&given, 3), waits_for(2), "what is behind it waits for it");
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::Time { until: given.now + 60, reason: Reason::WaitingForSpace }));
    assert_eq!(answer(&given, 5), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 5) }), "a step towards the folder is no try of a sending");
    given.now += 60;
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 2), parent: reference("root"), name: "d".into() }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 4) }));
}

/// New, as above: an item OneDrive refuses for good, or that is too big for the space
/// OneDrive has, waits for the user with that reason in place of any step towards OneDrive.
#[test]
fn an_item_whose_sending_onedrive_refused_waits_for_the_user() {
    let mut given = input(vec![made_directory(2, ROOT, "d"), made_file(3, id(2), "a.txt"), with_content_here(synced_file(4, ROOT, "b.txt"), written_here()), moved_in_onedrive(synced_file(5, ROOT, "c.txt"), ROOT, "c2.txt")]);
    given.sending_refusals = [(id(2), Reason::NotAllowed(None)), (id(4), Reason::WaitingForSpace), (id(5), Reason::NotAllowed(None))].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::NotAllowed(None))));
    assert_eq!(answer(&given, 3), waits_for(2));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::User(Reason::WaitingForSpace)));
    assert_eq!(answer(&given, 5), folder_step(FolderStep::MoveItem { item: versions(&given, 5) }));
}

/// New, as above: nothing of an item the check of the backing directory at start could not
/// decide is sent or taken: it waits for a check that decides, and so does what is behind
/// it. The removal of a directory does not take such an item with it.
#[test]
fn an_item_the_check_at_start_left_is_not_planned() {
    let mut given = input(vec![
        made_directory(2, ROOT, "d"),
        made_file(3, id(2), "a.txt"),
        with_content_in_onedrive(synced_file(4, ROOT, "b.txt"), hashed("theirs")),
        removed_here(synced_directory(5, ROOT, "e")),
        removed_here(synced_file(6, id(5), "f")),
        listed(7, ItemKind::File, ROOT, "new.txt"),
    ]);
    given.items_left_by_start_up_check = [id(2), id(4), id(6), id(7)].into();
    for left in [2, 4, 6, 7] {
        assert_eq!(answer(&given, left), Answer::Wait(WaitReason::StartUpCheck), "item {left}");
    }
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::StartUpCheck), "what is in an item the check left is held with it");
    assert_eq!(answer(&given, 5), waits_for(6));
}

/// Found by the larger convergence run (seed 310, the caller's facts drawn): a file both
/// sides agreed on, changed here and moved here within a directory the folder cannot show,
/// which OneDrive moved too. Its place waits for the user; its content is sent where
/// OneDrive has it, as for any item the folder cannot show (the brief's §7, M2).
#[test]
fn the_content_of_a_file_the_folder_cannot_show_whose_place_waits_for_the_user_is_sent() {
    let file = with_content_here(moved_in_onedrive(moved_here(synced_file(4, id(2), "c.txt"), id(3), "A"), id(2), "d"), written_here());
    let given = cannot_be_shown(input(vec![synced_directory(2, ROOT, "a"), synced_directory(3, id(2), "b"), file]), &[2, 3, 4]);
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendContent { item: versions(&given, 4) }));
    // With its content sent, it waits for the user.
    let file = moved_in_onedrive(moved_here(synced_file(4, id(2), "c.txt"), id(3), "A"), id(2), "d");
    let given = cannot_be_shown(input(vec![synced_directory(2, ROOT, "a"), synced_directory(3, id(2), "b"), file]), &[2, 3, 4]);
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::User(Reason::Parent)));
}

/// Found by the larger convergence run (seed 12, the caller's facts drawn), the class of
/// the review's M1: a step that is running sends an item into a directory — a move, a new
/// file, a new directory. The rows do not say so until its commit, and the removal of that
/// directory, or of one above it, would take the item with it: it waits for that step.
#[test]
fn the_removal_of_a_directory_waits_for_a_running_step_that_sends_an_item_into_it() {
    let rows = || vec![removed_here(synced_directory(2, ROOT, "a")), removed_here(synced_directory(3, id(2), "b")), synced_file(4, ROOT, "kept.txt")];
    let mut given = input(rows());
    given.items_with_running_step = [id(4)].into();
    given.destinations_of_running_steps = [id(3)].into();
    assert_eq!(answer(&given, 2), waits_for(3), "what is sent into the inner directory would go with the outer one");
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::RunningStep));
    given.destinations_of_running_steps = [id(2)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::RunningStep));
    assert_eq!(answer(&given, 3), waits_for(2));
    given.destinations_of_running_steps.clear();
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendDirectoryRemoval { item: versions(&given, 2), below: vec![versions(&given, 3)] }));
}

/// New, with the tries of the steps towards the folder kept by the caller: such a step
/// whose own next try is later waits until then, with the reason of its last failure, and
/// what is behind it waits for it; once that time has come it is planned again. A try of
/// another step of the item holds nothing: the try of a move does not hold the taking of
/// OneDrive's content, and a try of what was sent holds no step towards the folder.
#[test]
fn a_step_towards_the_folder_whose_own_next_try_is_later_waits_until_then() {
    let mut directory = listed(2, ItemKind::Directory, ROOT, "d");
    directory.synced_place = None;
    let mut given = input(vec![directory, listed(3, ItemKind::File, id(2), "a.txt"), with_content_in_onedrive(synced_file(5, ROOT, "c.txt"), hashed("theirs")), removed_in_onedrive(synced_file(6, ROOT, "gone.txt"))]);
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 6), folder_step(FolderStep::RemoveItem { item: versions(&given, 6) }));
    let later = |reason: Reason| NextTry { not_before: given.now + 60, reason };
    given.next_tries = [
        ((id(2), TriedStep::TowardsFolder(FolderStepKind::ShowItem)), later(Reason::Failed)),
        ((id(5), TriedStep::TowardsFolder(FolderStepKind::MoveItem)), later(Reason::Failed)),
        ((id(5), TriedStep::TowardsOneDrive), later(Reason::Network)),
        ((id(6), TriedStep::TowardsFolder(FolderStepKind::RemoveItem)), later(Reason::Changed)),
    ]
    .into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Time { until: given.now + 60, reason: Reason::Failed }));
    assert_eq!(answer(&given, 3), waits_for(2), "what is behind it waits for it");
    assert_eq!(answer(&given, 5), folder_step(FolderStep::TakeOneDriveContent { item: versions(&given, 5) }), "the tries of other steps of the item");
    assert_eq!(answer(&given, 6), Answer::Wait(WaitReason::Time { until: given.now + 60, reason: Reason::Changed }), "the step for an item OneDrive removed waits too");
    given.now += 60;
    assert_eq!(answer(&given, 2), folder_step(FolderStep::ShowItem { item: versions(&given, 2) }));
    assert_eq!(answer(&given, 6), folder_step(FolderStep::RemoveItem { item: versions(&given, 6) }));
}

/// As above, for a step of the rows alone and for a step a decision between items makes:
/// the hashing of a local content, and the renaming to a conflict copy, each waits for its
/// own next try.
#[test]
fn a_step_of_the_rows_and_a_step_made_between_items_wait_for_their_own_next_try() {
    let mut given = input(vec![made_file(2, ROOT, "a.txt"), listed(3, ItemKind::File, ROOT, "a.txt"), on_disk(made_file(4, ROOT, "b.txt")), listed(5, ItemKind::Directory, ROOT, "b.txt")]);
    let (hash, aside) = (answer(&given, 2), answer(&given, 4));
    assert_eq!(hash, row_step(RowStep::HashLocalContent { item: versions(&given, 2) }));
    assert!(matches!(aside, Answer::Step(SyncStep::Folder(FolderStep::RenameToConflictCopy { .. }))), "{aside:?}");
    let later = NextTry { not_before: given.now + 60, reason: Reason::Failed };
    given.next_tries = [((id(2), TriedStep::OfRows(RowStepKind::HashLocalContent)), later.clone()), ((id(4), TriedStep::TowardsFolder(FolderStepKind::RenameToConflictCopy)), later)].into();
    for item in [2, 4] {
        assert_eq!(answer(&given, item), Answer::Wait(WaitReason::Time { until: given.now + 60, reason: Reason::Failed }));
    }
    given.now += 60;
    assert_eq!((answer(&given, 2), answer(&given, 4)), (hash, aside));
}
