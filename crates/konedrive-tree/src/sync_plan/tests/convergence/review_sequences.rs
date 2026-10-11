//! The sequences of the reviews of the plan, as the reviews ran them: through the store,
//! the caller in miniature and the commits of the steps, from the tree every run begins
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

/// The second review's M1. The user removes their own file and `d` (`rm -r d`). Nothing is
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

/// The second review's M1, the rename (`mv d d-mine`): no move is sent for a directory the
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

/// The second review's M2, with the fact given as the input's doc says it is
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

/// The caller's clock goes on by `seconds`: the plan's `now` and the time the commits are
/// given.
fn pass_time(world: &mut World, seconds: i64) {
    world.facts.now += seconds;
    world.stand_in.now = world.facts.now;
}

/// Forty shown, the user's confirmation not given, and nobody ever began a window of sent
/// removals in this store.
fn world_of_forty_shown() -> World {
    let mut world = World::new(1);
    let listed: Vec<Change> = (0..31).map(|number| listed_file(&format!("M{number}"), "R", &format!("m{number}"), (600 + number, &format!("h{}-1000", 600 + number)), "e")).collect();
    write_listing_of_changes(&mut world.store, &listed);
    assert_eq!(run_and_tell_steps(&mut world).len(), 31);
    assert_eq!(world.store.item_store().count_shown_items().unwrap(), 40);
    world.facts.confirms_removals = false;
    pass_time(&mut world, 1_700_000_000);
    world
}

/// The second review's S2 (the first's S6), through the plan: six files are removed and
/// their removals sent; a listing begins and ends, as one does after every removal this
/// client sends; six more are removed a few seconds later. The twelve are one mass removal:
/// the second six wait for the user. Once the window's time has passed with no removal
/// sent, six are none.
#[test]
fn a_mass_removal_in_two_halves_with_a_listing_between_them_is_held() {
    let mut world = world_of_forty_shown();
    let files: Vec<LocalItemId> = (0..12).map(|number| find(&mut world, ROOT, &format!("m{number}"))).collect();
    for file in &files[..6] {
        remove(&mut world, *file);
    }
    assert_eq!(run_and_tell_steps(&mut world).len(), 6, "six of forty are no mass removal");
    write_listing_of_changes(&mut world.store, &[]);
    pass_time(&mut world, 5);
    assert_eq!(world.store.item_store().find_sent_removal_window(world.facts.now).unwrap().removal_count, 6);
    for file in &files[6..] {
        remove(&mut world, *file);
    }
    let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
    assert!(plan.steps.is_empty(), "{:?}", plan.steps);
    assert_eq!(plan.waits.iter().filter(|wait| wait.reason == WaitReason::User(Reason::MassDelete)).count(), 6);
    pass_time(&mut world, crate::SENT_REMOVAL_WINDOW_SECONDS);
    assert_eq!(run_and_tell_steps(&mut world).len(), 6);
}

/// The third review's S7, its sequence: one file is removed, planned, its removal
/// committed, a listing; fourteen times, in a store in which nobody ever began a window.
/// A day apart, each removal is a single one and is sent: no removal waits for the user
/// because others were once sent. Within a few seconds of one another, the tenth of forty
/// is a mass removal, and waits.
#[test]
fn single_removals_a_day_apart_are_never_held_and_those_of_a_short_time_are() {
    let mut world = world_of_forty_shown();
    let files: Vec<LocalItemId> = (0..31).map(|number| find(&mut world, ROOT, &format!("m{number}"))).collect();
    for (number, file) in files[..14].iter().enumerate() {
        remove(&mut world, *file);
        assert_eq!(run_and_tell_steps(&mut world), [format!("{file}:SendFileRemoval")], "removal {number}");
        write_listing_of_changes(&mut world.store, &[]);
        pass_time(&mut world, 86_400);
    }
    let mut held = Vec::new();
    for (number, file) in files[14..28].iter().enumerate() {
        remove(&mut world, *file);
        if run_and_tell_steps(&mut world).is_empty() {
            held.push(number);
        }
        write_listing_of_changes(&mut world.store, &[]);
        pass_time(&mut world, 2);
    }
    assert_eq!(held, [9, 10, 11, 12, 13], "the tenth is the first mass removal: nine sent and one unsent");
}

/// From the review of the check at start: after a store was rebuilt, the check binds an
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

/// Stage 0b's §2.1, through the store: the time of a clean copy is set here. No plan takes
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

/// The third review's S3, its sequence: `g` is removed here by itself; a listing says
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
