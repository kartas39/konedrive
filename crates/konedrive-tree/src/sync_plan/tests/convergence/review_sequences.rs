//! Sequences of changes, each run through the store, the caller in miniature and the
//! commits of the steps, from the tree every run begins
//! with — `a` {`b` {`c.txt`}, `c.txt`, `e.txt`}, `d` {`f`}, `g`, `c.txt`. Each says the
//! state it ends in: what OneDrive has, and what the folder shows.

use super::*;
use crate::item_store::tests::listed_drive::not_placed;

fn find(world: &mut World, parent: LocalItemId, name: &str) -> LocalItemId {
    world.store.item_store().find_item_at(parent, name.as_ref()).unwrap().unwrap_or_else(|| panic!("the folder does not show {name}")).local_item_id
}

fn row(world: &mut World, item: LocalItemId) -> Item {
    world.store.item_store().find_item(item).unwrap().unwrap_or_else(|| panic!("item {item} has no row"))
}

fn apply(world: &mut World, change: LocalVersionChange) -> LocalItemId {
    world.store.item_store().apply(&change).unwrap()
}

fn make_file(world: &mut World, parent: LocalItemId, name: &str) -> LocalItemId {
    let content = Some(ContentVersion::ChangedHere { size: 888, mtime: 5888, mtime_nsec: 5, change_count: 1 });
    apply(world, LocalVersionChange::Made { kind: ItemKind::File, place: place(parent, name), content, is_name_ignored: false, replaced: None })
}

fn move_here(world: &mut World, item: LocalItemId, parent: LocalItemId, name: &str) {
    apply(world, LocalVersionChange::Moved { item, place: place(parent, name), is_name_ignored: false });
}

fn remove(world: &mut World, item: LocalItemId) {
    apply(world, LocalVersionChange::Removed { item });
}

fn onedrive_place(row: &Item) -> Option<ItemPlace> {
    row.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone())
}

fn local_place(row: &Item) -> Option<ItemPlace> {
    row.local.as_ref().and_then(|local| local.place.clone())
}

/// The plans made until one has no step, each with its steps run: every step that ran, by
/// its kind and its item.
fn run_and_tell_steps(world: &mut World) -> Vec<String> {
    let mut ran = Vec::new();
    for _ in 0..ROUNDS {
        let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
        assert!(plan.faults.is_empty(), "{:?}", plan.faults);
        if plan.steps.is_empty() {
            return ran;
        }
        for step in &plan.steps {
            let kind = format!("{step:?}");
            let kind = kind.split(['(', ' ']).nth(1).unwrap_or_default().to_owned();
            assert_eq!(world.stand_in.run(&mut world.store, step), StepOutcome::Written, "{step:?}");
            ran.push(format!("{}:{kind}", step.local_item_id()));
            assert_eq!(find_broken_rule(&rows(&mut world.store), false), None, "after {step:?}");
        }
    }
    panic!("it does not end: {ran:?}");
}

/// `d` holds the user's own file, and a listing says the folder cannot show `d`: `f` goes
/// here, and `d` stays in the folder, with `mine.txt` alone, for as long as the user
/// leaves it. The ids of `d`, `f` and `mine.txt`.
fn world_with_the_users_own_in_a_directory_the_folder_cannot_show() -> (World, [LocalItemId; 3]) {
    let mut world = World::new(1);
    let d = find(&mut world, ROOT, "d");
    let f = find(&mut world, d, "f");
    let mine = make_file(&mut world, d, "mine.txt");
    write_listing_of_changes(&mut world.store, &[not_placed(listed_directory("D3", "R", "d", "e-flipped"))]);
    assert_eq!(run_and_tell_steps(&mut world), [format!("{f}:RemoveItem")]);
    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    let waits: Vec<(LocalItemId, WaitReason)> = plan.waits.into_iter().map(|wait| (wait.item, wait.reason)).collect();
    assert_eq!(waits, [(d, WaitReason::AnotherItem(mine)), (mine, WaitReason::User(Reason::Parent))]);
    (world, [d, f, mine])
}

/// The user removes their own file and `d` (`rm -r d`). Nothing is
/// sent: OneDrive has below `d` what the folder never showed when `d` was removed — the
/// plan itself had taken `f` off the folder.
///
/// End: OneDrive has `d` and `d/f`, as it had them. The folder shows neither, and nothing
/// is agreed on of either: both are items OneDrive has that the folder cannot show.
#[test]
fn a_directory_the_folder_cannot_show_removed_here_with_the_users_own_is_not_removed_from_onedrive() {
    let (mut world, [d, f, mine]) = world_with_the_users_own_in_a_directory_the_folder_cannot_show();
    remove(&mut world, mine);
    remove(&mut world, d);
    assert_eq!(run_and_tell_steps(&mut world), [format!("{d}:ForgetSyncedVersion")]);
    let (d_row, f_row) = (row(&mut world, d), row(&mut world, f));
    assert_eq!((onedrive_place(&d_row), onedrive_place(&f_row)), (Some(place(ROOT, "d")), Some(place(d, "f"))), "OneDrive has both");
    for row in [&d_row, &f_row] {
        assert_eq!((&row.local, &row.synced_place, &row.synced_content, row.unsent_since), (&None, &None, &None, None), "{row:?}");
    }
    assert!(world.store.item_store().find_item(mine).unwrap().is_none(), "what the user made and removed has no row");
    let plan = make_sync_plan(&describe_every_item(&mut world.store, &world.facts));
    assert_eq!((plan.steps.len(), plan.waits.len(), plan.faults.len()), (0, 0, 0), "{plan:?}");
}

/// `mv d d-mine`: no move is sent for a directory the
/// folder cannot show.
///
/// End: OneDrive has `d` under its name, with `f`. The folder shows `d-mine/mine.txt`,
/// which waits for the user; once the user moves their own out of it, the directory goes
/// here, the file is sent as a new one, and OneDrive has `d` still.
#[test]
fn a_directory_the_folder_cannot_show_renamed_here_is_not_renamed_in_onedrive() {
    let (mut world, [d, f, mine]) = world_with_the_users_own_in_a_directory_the_folder_cannot_show();
    move_here(&mut world, d, ROOT, "d-mine");
    assert_eq!(run_and_tell_steps(&mut world), Vec::<String>::new());
    let d_row = row(&mut world, d);
    assert_eq!((local_place(&d_row), onedrive_place(&d_row)), (Some(place(ROOT, "d-mine")), Some(place(ROOT, "d"))));
    assert_eq!(local_place(&row(&mut world, mine)), Some(place(d, "mine.txt")));

    move_here(&mut world, mine, ROOT, "mine.txt");
    let ran = run_and_tell_steps(&mut world);
    assert!(ran.contains(&format!("{mine}:SendNewFile")) && ran.contains(&format!("{d}:RemoveItem")) && !ran.iter().any(|step| step.ends_with("SendMove") || step.ends_with("Removal")), "{ran:?}");
    let (d_row, f_row, mine_row) = (row(&mut world, d), row(&mut world, f), row(&mut world, mine));
    assert_eq!((onedrive_place(&d_row), onedrive_place(&f_row), d_row.local.clone()), (Some(place(ROOT, "d")), Some(place(d, "f")), None));
    assert!(mine_row.onedrive.is_some() && mine_row.local == mine_row.onedrive);
}

/// With the fact given as the input's doc says it is
/// ([`SyncPlanInput::destinations_of_running_steps`]). `mv g d/g`: the request moves `g`
/// into `d` in OneDrive, and its answer is lost — no commit, and the request stays on
/// record, not settled. `mv d/g g`; remove `d/f` and `d`. While the record stands, `g` has
/// a step running and `d` is a destination of one: no removal of `d` is sent. The record
/// is settled by reading `g` from OneDrive, which says it is in `d`: the plan sends it out
/// first, and only then the removal.
///
/// End: OneDrive has `g` in the root, as the folder shows it, and the two sides agree; `d`
/// and `d/f` are gone from both.
#[test]
fn a_move_into_a_directory_whose_answer_was_lost_holds_the_directorys_removal_until_it_is_settled() {
    let mut world = World::new(1);
    let (d, g) = (find(&mut world, ROOT, "d"), find(&mut world, ROOT, "g"));
    let f = find(&mut world, d, "f");
    move_here(&mut world, g, d, "g");
    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    let [SyncStep::OneDrive(OneDriveStep::SendMove { parent, .. })] = plan.steps.as_slice() else { panic!("{:?}", plan.steps) };
    assert_eq!(parent, &onedrive_reference("D3"));
    // The request left and went through; its answer did not come. It is on record.
    world.facts.items_with_running_step.insert(g);
    world.facts.destinations_of_running_steps.insert(d);
    move_here(&mut world, g, ROOT, "g");
    remove(&mut world, f);
    remove(&mut world, d);
    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    assert!(plan.steps.is_empty(), "{:?}", plan.steps);
    let waits: BTreeMap<LocalItemId, WaitReason> = plan.waits.into_iter().map(|wait| (wait.item, wait.reason)).collect();
    // By its row `g` agrees on everything: only the record says OneDrive may have it in `d`.
    assert_eq!((waits.get(&d), waits.get(&g), waits.get(&f)), (Some(&WaitReason::RunningStep), None, Some(&WaitReason::AnotherItem(d))));
    let (planned_from, below) = (ItemVersions::of(&row(&mut world, d)), [ItemVersions::of(&row(&mut world, f))]);
    assert!(!world.store.item_store().can_directory_removal_be_sent(&planned_from, &below, &world.facts.items_with_running_step, &world.facts.destinations_of_running_steps).unwrap(), "nor does the step's own guard let it leave");

    // The record is settled by reading the item: OneDrive has `g` in `d`.
    write_listing_of_changes(&mut world.store, &[listed_file("F5", "D3", "g", (15, "h15-1000"), "e-lost")]);
    world.facts.items_with_running_step.clear();
    world.facts.destinations_of_running_steps.clear();
    assert_eq!(run_and_tell_steps(&mut world), [format!("{g}:SendMove"), format!("{d}:SendDirectoryRemoval")]);
    let g_row = row(&mut world, g);
    assert_eq!((local_place(&g_row), onedrive_place(&g_row), g_row.synced_place.clone(), g_row.unsent_since), (Some(place(ROOT, "g")), Some(place(ROOT, "g")), Some(place(ROOT, "g")), None));
    for gone in [d, f] {
        let gone = row(&mut world, gone);
        assert_eq!((&gone.local, &gone.onedrive, &gone.synced_place), (&None, &None, &None), "{gone:?}");
    }
}

/// After a store was rebuilt, the check at start binds an
/// entry to the item of OneDrive its mark names; the entry is a copy of that file at
/// another place (`d/a.txt`), and a listing says the item is at `a/a.txt`. No move is sent
/// and the entry is not moved: the binding was a guess, and is given up.
///
/// End: the folder shows `a/a.txt` as OneDrive has it, not on disk; `d/a.txt` is the
/// folder's own file, sent as a new item; the item the mark named is in OneDrive where it
/// was, under the eTag the listing said — nothing was moved or removed there.
#[test]
fn an_entry_bound_by_its_mark_at_another_place_than_onedrive_has_the_item_gives_the_binding_up() {
    let mut world = World::new(1);
    let (a, d) = (find(&mut world, ROOT, "a"), find(&mut world, ROOT, "d"));
    let copy = make_file(&mut world, d, "a.txt");
    world.store.item_store().bind_item(copy, &onedrive_reference("Z1")).unwrap();
    write_listing_of_changes(&mut world.store, &[listed_file("Z1", "D1", "a.txt", (41, "h41-1000"), "e-Z1")]);
    let bound = row(&mut world, copy);
    assert_eq!((local_place(&bound), onedrive_place(&bound), bound.synced_place.clone()), (Some(place(d, "a.txt")), Some(place(a, "a.txt")), None));

    let ran = run_and_tell_steps(&mut world);
    assert_eq!(ran[0], format!("{copy}:GiveUpBinding"));
    assert!(ran.contains(&format!("{copy}:SendNewFile")) && !ran.iter().any(|step| step.ends_with("Move") || step.ends_with("MoveItem") || step.ends_with("Removal") || step.ends_with("RemoveItem")), "{ran:?}");
    let listed = world.store.item_store().find_bound_item(&onedrive_reference("Z1")).unwrap().expect("the row of the item the mark named");
    assert_eq!((local_place(&listed), onedrive_place(&listed), listed.onedrive_etag.as_deref(), listed.content_presence), (Some(place(a, "a.txt")), Some(place(a, "a.txt")), Some("e-Z1"), ContentPresence::NotOnDisk));
    let own = row(&mut world, copy);
    assert!(own.onedrive_item.is_some() && own.onedrive_item != listed.onedrive_item && own.local == own.onedrive, "{own:?}");
    assert_eq!((local_place(&own), content_of(&own.local)), (Some(place(d, "a.txt")), Some("h888-5888".to_owned())));
}

/// Through the store: the time of a clean copy is set here. No plan takes
/// it back: nothing is to do, the row keeps the time that was set, on both the local
/// content and the content agreed on, and OneDrive's keeps OneDrive's.
#[test]
fn a_time_set_on_a_clean_copy_is_not_taken_back_by_the_plan() {
    let mut world = World::new(1);
    let g = find(&mut world, ROOT, "g");
    world.store.item_store().set_content_presence(g, ContentPresence::OnDisk).unwrap();
    let before = row(&mut world, g);
    world.store.item_store().set_local_modified_time(g, 5000, 7).unwrap();

    assert_eq!(run_and_tell_steps(&mut world), Vec::<String>::new());
    let plan = make_sync_plan(&describe_every_item(&mut world.store, &world.facts));
    assert_eq!((plan.steps, plan.waits, plan.faults), (vec![], vec![], vec![]), "nor does a plan given every row");
    let after = row(&mut world, g);
    let time = |content: Option<&ContentVersion>| content.map(ContentVersion::mtime);
    assert_eq!((time(after.local.as_ref().and_then(|local| local.content.as_ref())), time(after.synced_content.as_ref())), (Some((5000, 7)), Some((5000, 7))));
    assert_eq!((&after.onedrive, after.unsent_since, after.content_presence), (&before.onedrive, None, ContentPresence::OnDisk));
}

/// `g` is removed here by itself; a listing says
/// OneDrive moved it into `d` and changed its content; `d/f` and `d` are removed here. The
/// content OneDrive alone has is not removed with `d`: the removal of `d` is of the folder
/// alone.
///
/// End: OneDrive has `d` and `d/g` with the new content, and the folder shows both, `g`
/// not on disk; `d/f`, which the user removed, is gone from both sides.
#[test]
fn a_file_removed_here_whose_content_onedrive_changed_and_moved_into_a_removed_directory_comes_back() {
    let mut world = World::new(1);
    let (d, g) = (find(&mut world, ROOT, "d"), find(&mut world, ROOT, "g"));
    let f = find(&mut world, d, "f");
    remove(&mut world, g);
    write_listing_of_changes(&mut world.store, &[listed_file("F5", "D3", "g", (99, "h99"), "e-F5-2")]);
    remove(&mut world, f);
    remove(&mut world, d);

    let ran = run_and_tell_steps(&mut world);
    assert!(!ran.iter().any(|step| step.ends_with("SendDirectoryRemoval")), "{ran:?}");
    assert_eq!(sorted(ran), sorted(vec![format!("{f}:SendFileRemoval"), format!("{d}:ForgetSyncedVersion"), format!("{d}:ShowItem"), format!("{g}:ShowItem")]));
    let (d_row, g_row, f_row) = (row(&mut world, d), row(&mut world, g), row(&mut world, f));
    assert_eq!((local_place(&g_row), onedrive_place(&g_row), g_row.synced_place.clone(), g_row.content_presence), (Some(place(d, "g")), Some(place(d, "g")), Some(place(d, "g")), ContentPresence::NotOnDisk));
    assert_eq!((content_of(&g_row.local), content_of(&g_row.onedrive), g_row.unsent_since), (Some("h99".to_owned()), Some("h99".to_owned()), None), "the content OneDrive alone had is shown here");
    assert_eq!((local_place(&d_row), onedrive_place(&d_row), d_row.synced_place.clone()), (Some(place(ROOT, "d")), Some(place(ROOT, "d")), Some(place(ROOT, "d"))));
    assert_eq!((&f_row.local, &f_row.onedrive, &f_row.synced_place), (&None, &None, &None), "what the user removed in it is removed");
}

fn sorted(mut steps: Vec<String>) -> Vec<String> {
    steps.sort();
    steps
}

/// A file made here and one OneDrive made under its name with the same content, merged
/// into one item by a step that was just written: the id of the row. No plan was made
/// since the merge.
fn merge_new_file_with_onedrives(world: &mut World) -> LocalItemId {
    let x = make_file(world, ROOT, "x");
    write_listing_of_changes(&mut world.store, &[listed_file("X1", "R", "x", (888, "h888-5888"), "e-X1")]);
    for _ in 0..ROUNDS {
        let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
        assert!(!plan.steps.is_empty(), "no merge was planned");
        for step in &plan.steps {
            assert_eq!(world.stand_in.run(&mut world.store, step), StepOutcome::Written, "{step:?}");
        }
        if plan.steps.iter().any(|step| matches!(step, SyncStep::Row(RowStep::MergeItems { .. }))) {
            assert_eq!(row(world, x).onedrive_item, Some(onedrive_reference("X1")));
            return x;
        }
    }
    panic!("no merge was planned");
}

/// A merge was just written, and then OneDrive
/// renames the item, or the user does. The merge is something agreed on, so the binding is
/// no guess: the rename is a move of one item, towards the folder or towards OneDrive, and
/// no second item is made on either side.
#[test]
fn an_item_renamed_on_either_side_right_after_its_merge_is_moved_and_not_sent_as_new() {
    let mut world = World::new(1);
    let x = merge_new_file_with_onedrives(&mut world);
    write_listing_of_changes(&mut world.store, &[listed_file("X1", "R", "y", (888, "h888-5888"), "e-X1-2")]);
    let ran = run_and_tell_steps(&mut world);
    assert!(ran.contains(&format!("{x}:MoveItem")) && !ran.iter().any(|step| step.ends_with("GiveUpBinding") || step.ends_with("SendNewFile") || step.ends_with("ShowItem")), "{ran:?}");
    let moved = row(&mut world, x);
    assert_eq!((local_place(&moved), onedrive_place(&moved), moved.synced_place.clone(), moved.onedrive_item.clone()), (Some(place(ROOT, "y")), Some(place(ROOT, "y")), Some(place(ROOT, "y")), Some(onedrive_reference("X1"))));

    let mut world = World::new(1);
    let x = merge_new_file_with_onedrives(&mut world);
    move_here(&mut world, x, ROOT, "x-renamed");
    let ran = run_and_tell_steps(&mut world);
    assert!(ran.contains(&format!("{x}:SendMove")) && !ran.iter().any(|step| step.ends_with("GiveUpBinding") || step.ends_with("SendNewFile") || step.ends_with("ShowItem")), "{ran:?}");
    let moved = row(&mut world, x);
    assert_eq!((local_place(&moved), onedrive_place(&moved), moved.onedrive_item.clone()), (Some(place(ROOT, "x-renamed")), Some(place(ROOT, "x-renamed")), Some(onedrive_reference("X1"))));
    assert_eq!(world.store.item_store().find_item_at(ROOT, "x".as_ref()).unwrap(), None, "no second item under the old name");
}

/// An entry `copy` the check at start bound by its
/// mark to the directory OneDrive has as `a/docs`, with the user's own `new.txt` in it.
/// The binding is a guess and is given up; the file waits for that, and is sent once, into
/// the directory the folder's `copy` is made as. Nothing of the user's is ever sent into
/// the item the binding was of.
#[test]
fn a_new_file_in_a_directory_that_gives_up_a_guessed_binding_is_not_sent_into_the_item_the_binding_was_of() {
    let mut world = World::new(1);
    let a = find(&mut world, ROOT, "a");
    let copy = apply(&mut world, LocalVersionChange::Made { kind: ItemKind::Directory, place: place(ROOT, "copy"), content: None, is_name_ignored: false, replaced: None });
    world.store.item_store().bind_item(copy, &onedrive_reference("Q1")).unwrap();
    let new = make_file(&mut world, copy, "new.txt");
    write_listing_of_changes(&mut world.store, &[listed_directory("Q1", "D1", "docs", "e-Q1")]);
    assert_eq!((local_place(&row(&mut world, copy)), onedrive_place(&row(&mut world, copy))), (Some(place(ROOT, "copy")), Some(place(a, "docs"))));

    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    let steps: Vec<String> = plan.steps.iter().map(|step| format!("{}:{:?}", step.local_item_id(), std::mem::discriminant(step))).collect();
    assert_eq!(plan.steps.len(), 1, "{steps:?}");
    assert!(matches!(&plan.steps[0], SyncStep::Row(RowStep::GiveUpBinding { item }) if item.local_item_id == copy), "{:?}", plan.steps);
    assert_eq!(plan.waits.iter().find(|wait| wait.item == new).map(|wait| wait.reason.clone()), Some(WaitReason::AnotherItem(copy)));

    let ran = run_and_tell_steps(&mut world);
    assert_eq!(ran.iter().filter(|step| step.starts_with(&format!("{new}:"))).collect::<Vec<_>>(), [&format!("{new}:SendNewFile")], "{ran:?}");
    let made = ran.iter().position(|step| *step == format!("{copy}:MakeDirectory")).expect("the folder's directory is made in OneDrive");
    assert!(made < ran.iter().position(|step| *step == format!("{new}:SendNewFile")).unwrap(), "{ran:?}");
    let (copy_row, new_row) = (row(&mut world, copy), row(&mut world, new));
    assert!(copy_row.onedrive_item.is_some() && copy_row.onedrive_item != Some(onedrive_reference("Q1")));
    assert_eq!((local_place(&new_row), onedrive_place(&new_row)), (Some(place(copy, "new.txt")), Some(place(copy, "new.txt"))));
    let docs = world.store.item_store().find_bound_item(&onedrive_reference("Q1")).unwrap().unwrap();
    assert_eq!(world.store.item_store().list_children_in_onedrive(docs.local_item_id).unwrap(), [], "OneDrive's directory holds nothing of the user's");
}

// -- what the steps remember: a temporary name, what an upload left, an own upload's content

/// The plan made of every row has nothing left: no step, no wait, no fault, and no item a
/// rule leaves out.
fn assert_nothing_is_left_to_plan(world: &mut World) {
    let plan = make_sync_plan(&describe_every_item(&mut world.store, &world.facts));
    assert_eq!((plan.steps.len(), plan.waits.len(), plan.faults.len(), plan.exclusions.len()), (0, 0, 0, 0), "{plan:?}");
}

/// The one step of the next plan that sends an item under a temporary name: the item, and
/// the name.
fn plan_sending_under_temporary_name(world: &mut World) -> (LocalItemId, std::ffi::OsString) {
    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    let under_temporary_name: Vec<(LocalItemId, std::ffi::OsString)> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            SyncStep::OneDrive(OneDriveStep::SendMove { item, name, .. } | OneDriveStep::MakeDirectory { item, name, .. }) if name.as_encoded_bytes().starts_with(b".konedrive-swap-") => Some((item.local_item_id, name.clone())),
            _ => None,
        })
        .collect();
    let [sent] = under_temporary_name.as_slice() else { panic!("no one item goes through a temporary name: {:?}", plan.steps) };
    sent.clone()
}

/// The exchange of names killed in the middle. `mv g t; mv c.txt g; mv t c.txt`: the plan
/// sends one of the two to its temporary name. That request goes through — the steps
/// remember the name, OneDrive has the file under it — and the daemon dies before the
/// commit; the listing after the start names the file there. The plans that follow send
/// the other file to the name that is free now, and then this one on from its temporary
/// name: no step brings the folder to that name.
///
/// End: OneDrive has the two files under each other's names, as the folder shows them; the
/// two sides agree, and nothing is remembered.
#[test]
fn an_exchange_of_names_killed_under_the_temporary_name_ends_with_the_names_exchanged() {
    let mut world = World::new(1);
    let (g, c) = (find(&mut world, ROOT, "g"), find(&mut world, ROOT, "c.txt"));
    move_here(&mut world, g, ROOT, "t");
    move_here(&mut world, c, ROOT, "g");
    move_here(&mut world, g, ROOT, "c.txt");
    let (through, temporary_name) = plan_sending_under_temporary_name(&mut world);
    let other = if through == g { c } else { g };
    let sent = row(&mut world, through);
    let toward = local_place(&sent).unwrap();
    world.store.step_memory().remember_temporary_name(through, &crate::TemporaryName { name: temporary_name.clone(), toward }).unwrap();
    let (item_id, size) = (sent.onedrive_item.as_ref().unwrap().item_id.as_str().to_owned(), sent.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref()).unwrap().size());
    write_listing_of_changes(&mut world.store, &[listed_file(&item_id, "R", &temporary_name.to_string_lossy(), (size, &format!("h{size}-1000")), "e-under-temporary-name")]);
    assert_eq!(onedrive_place(&row(&mut world, through)), Some(place(ROOT, &temporary_name.to_string_lossy())));
    assert!(world.store.item_store().reader().find_item_memory(through).unwrap().temporary_name.is_some(), "a listing that names the item under the name keeps it");

    assert_eq!(run_and_tell_steps(&mut world), [format!("{other}:SendMove"), format!("{through}:SendMove")]);
    for (item, name) in [(g, "c.txt"), (c, "g")] {
        let row = row(&mut world, item);
        assert_eq!((local_place(&row), onedrive_place(&row), row.synced_place.clone(), row.unsent_since), (Some(place(ROOT, name)), Some(place(ROOT, name)), Some(place(ROOT, name)), None));
    }
    assert!(world.store.item_store().reader().find_item_memory(through).unwrap().temporary_name.is_none());
    assert_nothing_is_left_to_plan(&mut world);
}

/// `mv c.txt t; mkdir c.txt; mv t c.txt/c.txt`: the new directory
/// is made in OneDrive under its temporary name, as the name it takes is the file's still.
/// The request goes through and its answer is lost; after the start a listing makes a row
/// of the directory OneDrive has under that name. The plan merges the two rows, sends the
/// file into the directory, and the directory on to its name.
///
/// End: OneDrive has the directory `c.txt` with the file `c.txt` in it, as the folder
/// shows them, made once; no row is left of the one the listing made.
#[test]
fn a_directory_made_under_its_temporary_name_whose_answer_was_lost_is_merged_and_sent_on() {
    let mut world = World::new(1);
    let file = find(&mut world, ROOT, "c.txt");
    move_here(&mut world, file, ROOT, "t");
    let directory = apply(&mut world, LocalVersionChange::Made { kind: ItemKind::Directory, place: place(ROOT, "c.txt"), content: None, is_name_ignored: false, replaced: None });
    move_here(&mut world, file, directory, "c.txt");
    let (through, temporary_name) = plan_sending_under_temporary_name(&mut world);
    assert_eq!(through, directory);
    world.store.step_memory().remember_temporary_name(directory, &crate::TemporaryName { name: temporary_name.clone(), toward: place(ROOT, "c.txt") }).unwrap();
    write_listing_of_changes(&mut world.store, &[listed_directory("MADE", "R", &temporary_name.to_string_lossy(), "e-made")]);
    let listed = world.store.item_store().find_bound_item(&onedrive_reference("MADE")).unwrap().unwrap().local_item_id;
    assert!(listed != directory);

    assert_eq!(run_and_tell_steps(&mut world), [format!("{directory}:MergeItems"), format!("{file}:SendMove"), format!("{directory}:SendMove")]);
    assert!(world.store.item_store().find_item(listed).unwrap().is_none(), "the listed row went with the merge");
    let (directory_row, file_row) = (row(&mut world, directory), row(&mut world, file));
    assert_eq!(directory_row.onedrive_item, Some(onedrive_reference("MADE")), "made once");
    assert_eq!((local_place(&directory_row), onedrive_place(&directory_row), directory_row.unsent_since), (Some(place(ROOT, "c.txt")), Some(place(ROOT, "c.txt")), None));
    assert_eq!((local_place(&file_row), onedrive_place(&file_row), file_row.unsent_since), (Some(place(directory, "c.txt")), Some(place(directory, "c.txt")), None));
    assert_nothing_is_left_to_plan(&mut world);
}

/// What an upload left. The upload of the new file `new.txt` ends with other content in
/// OneDrive than was sent: the steps remember the item it left, and a listing makes a row
/// of it. That row is not shown, and the file is neither merged with it nor renamed for
/// it: it is sent again under its name. The step removes what was left first, and the
/// file is then sent.
///
/// End: OneDrive has the file once, with the content the folder has; of what the upload
/// left a binding alone is kept.
#[test]
fn a_file_whose_upload_left_other_content_is_sent_again_and_what_was_left_is_never_shown() {
    let mut world = World::new(1);
    let file = make_file(&mut world, ROOT, "new.txt");
    let left = onedrive_reference("LEFT");
    let standing = world.store.step_memory().remember_mismatched_item(file, &crate::MismatchedItem::of_answer(left.clone(), Some("c-h-left"), None)).unwrap();
    assert_eq!(standing, crate::MismatchedItemStanding::LeftToRemove);
    write_listing_of_changes(&mut world.store, &[listed_file("LEFT", "R", "new.txt", (888, "h-left"), "e-left")]);
    let left_row = world.store.item_store().find_bound_item(&left).unwrap().unwrap().local_item_id;

    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    assert_eq!(plan.exclusions, [ItemExclusion { item: left_row, reason: ExclusionReason::LeftByUpload }]);
    let [SyncStep::OneDrive(OneDriveStep::SendNewFile { item, name, .. })] = plan.steps.as_slice() else { panic!("{:?}", plan.steps) };
    assert_eq!((item.local_item_id, name.to_str()), (file, Some("new.txt")));
    assert!(plan.waits.is_empty() && plan.faults.is_empty(), "{plan:?}");

    // The step removes what its upload left before it sends the file again.
    world.store.step_memory().forget_removed_mismatched_item(&left).unwrap();
    assert_eq!(run_and_tell_steps(&mut world), [format!("{file}:SendNewFile")]);
    let (file_row, left_row) = (row(&mut world, file), row(&mut world, left_row));
    assert_eq!((local_place(&file_row), onedrive_place(&file_row), file_row.unsent_since), (Some(place(ROOT, "new.txt")), Some(place(ROOT, "new.txt")), None));
    assert_eq!((&left_row.local, &left_row.onedrive, &left_row.synced_place), (&None, &None, &None), "nothing of it was ever shown");
    assert_nothing_is_left_to_plan(&mut world);
}

/// An own upload's content. `a/e.txt` is written here; its upload ends with another hash in
/// OneDrive than was sent, the store keeps that content as the folder's own upload, and a
/// listing writes it into the row. The plan sends the local content again: OneDrive's is
/// not taken, the local content is not hashed against it, and no conflict copy is made.
///
/// End: the two sides agree on the content written here, and the fact says nothing more.
#[test]
fn a_content_the_own_upload_left_in_onedrive_is_sent_again_and_confirmed() {
    let mut world = World::new(1);
    let a = find(&mut world, ROOT, "a");
    let file = find(&mut world, a, "e.txt");
    apply(&mut world, LocalVersionChange::OpenedForWriting { item: file });
    apply(&mut world, LocalVersionChange::ClosedChanged { item: file, size: 99, mtime: 7000, mtime_nsec: 5 });
    write_listing_of_changes(&mut world.store, &[listed_file("F2", "D1", "e.txt", (99, "h-left"), "e-left")]);
    let written = row(&mut world, file);
    let content = |version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.content.clone()).unwrap();
    let own_upload = crate::OwnUploadContent { onedrive_content: content(&written.onedrive), local_content: content(&written.local) };
    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    assert!(matches!(plan.steps.as_slice(), [SyncStep::Row(RowStep::HashLocalContent { .. })]), "not told, as if OneDrive had changed the file: {:?}", plan.steps);

    world.store.item_store().remember_own_upload_content(file, &own_upload).unwrap();
    assert_eq!(run_and_tell_steps(&mut world), [format!("{file}:SendContent")]);
    let sent = row(&mut world, file);
    assert_eq!((hash_of(&content(&sent.local)), hash_of(&content(&sent.onedrive)), sent.unsent_since), ("h99-7000".to_owned(), "h99-7000".to_owned(), None));
    assert_eq!((local_place(&sent), sent.synced_content.is_some()), (Some(place(a, "e.txt")), true));
    assert_eq!(world.store.item_store().reader().find_own_upload_content(file).unwrap(), None);
    assert_nothing_is_left_to_plan(&mut world);
}
