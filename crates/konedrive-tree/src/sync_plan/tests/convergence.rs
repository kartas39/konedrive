//! The plan converges: the rows of a store, changed at random on both sides, are planned and
//! the steps run until no step is left, and then the two sides are equal and nothing was
//! lost (`docs/tasks/item-versions-design.md`, §12).
//!
//! Every row is made by an operation of [`ItemStore`](crate::ItemStore): a change of the
//! folder by `apply`, a change of OneDrive by a listing, and what the two agree on by the
//! commit of a step. OneDrive and the folder are nothing but those commits, given plausible
//! answers ([`step_stand_in`]): OneDrive has what the rows say it has. The plan is given
//! what a caller reads from the store's bounded readers ([`plan_input`]).
//!
//! What is asked, of every run:
//!
//! 1. it ends within a bound of rounds, with no step left, no fault, and no step whose
//!    commit finds the rows changed or whose name OneDrive answers is taken: the steps of
//!    one plan run in an order made at random;
//! 2. the rules of the rows hold after every step;
//! 3. at the end every item has its three versions equal, but an item that is left here and
//!    what is in one;
//! 4. nothing is lost ([`ExpectedEnd`]): every content that one side alone had, new or
//!    changed there, is on both sides under some name, and every item one side alone had is
//!    on both sides. One loss is the design's own and is left out: what OneDrive made or
//!    changed in a directory that was removed here goes with that removal, which stands;
//! 5. a plan over every row of what has converged has no step.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::row_rules::find_broken_rule;
use crate::model::{Change, LocalVersionChange};
use crate::{ListingLink, TreeStore};

/// The caller of the plan in miniature: what it reads and gives.
mod plan_input;
/// Changes made at random on both sides, from a seed.
mod random_changes;
/// The stand-in for OneDrive and the folder: the commits of the steps.
mod step_stand_in;

use plan_input::{describe_every_item, plan_next_steps, CallerFacts, WideningCount};
use random_changes::{OneDriveChanges, RandomChanges, RandomSource};
use step_stand_in::{hash_of, StepOutcome, StepStandIn};

/// More rounds than any run needs: a run that does not end within them does not end.
const ROUNDS: usize = 60;

/// A store with the two sides of it, and what a run counts.
struct World {
    store: TreeStore,
    stand_in: StepStandIn,
    facts: CallerFacts,
    changes: RandomChanges,
    /// The order the steps of one plan run in.
    step_order: RandomSource,
    widening: WideningCount,
    listing_count: u64,
    round_count: usize,
}

type Failure = String;

fn rows(store: &mut TreeStore) -> Vec<Item> {
    store.item_store().list_items().unwrap()
}

/// The rows in a line each: what a failure is read by.
fn describe_rows(rows: &[Item]) -> String {
    let place = |place: Option<&ItemPlace>| place.map_or_else(|| "-".to_owned(), |place| format!("{}/{}", place.parent_local_item_id, place.name.to_string_lossy()));
    let content = |content: Option<&ContentVersion>| content.map_or_else(String::new, |content| format!(" {}", hash_of(content)));
    let version = |version: &Option<ItemVersion>| version.as_ref().map_or_else(|| "-".to_owned(), |version| format!("{}{}", place(version.place.as_ref()), content(version.content.as_ref())));
    rows.iter()
        .map(|row| {
            format!(
                "  {} {:?} {} here {} | OneDrive {} | agreed {}{} | {:?}{}\n",
                row.local_item_id,
                row.kind,
                row.onedrive_item.as_ref().map_or("-", |reference| reference.item_id.as_str()),
                version(&row.local),
                version(&row.onedrive),
                place(row.synced_place.as_ref()),
                content(row.synced_content.as_ref()),
                row.content_presence,
                if row.is_name_ignored { " ignored" } else { "" }
            )
        })
        .collect()
}

impl World {
    /// A store whose drive is known, with a few directories and files both sides agree on,
    /// which the plan itself showed here.
    fn new(seed: u64) -> Self {
        let mut world = Self {
            store: store_with_listed_root(),
            stand_in: StepStandIn::new(),
            facts: CallerFacts::of_read_write_folder(),
            changes: RandomChanges::new(seed),
            step_order: RandomSource::new(seed ^ 0x5EED),
            widening: WideningCount::default(),
            listing_count: 0,
            round_count: 0,
        };
        let file = |id: &str, parent: &str, name: &str, size: u64| listed_file(id, parent, name, (size, &format!("h{size}-1000")), "e");
        let tree = [
            listed_directory("D1", "R", "a", "e"),
            listed_directory("D2", "D1", "b", "e"),
            listed_directory("D3", "R", "d", "e"),
            file("F1", "D1", "c.txt", 11),
            file("F2", "D1", "e.txt", 12),
            file("F3", "D2", "c.txt", 13),
            file("F4", "D3", "f", 14),
            file("F5", "R", "g", 15),
            file("F6", "R", "c.txt", 16),
        ];
        write_listing_of_changes(&mut world.store, &tree);
        let expected = ExpectedEnd::of(&mut world.store);
        world.run_to_the_end().and_then(|_| world.check_converged(&expected)).unwrap_or_else(|failure| panic!("the tree the runs begin with: {failure}"));
        world
    }

    /// Makes plans and runs their steps until a plan has none, for `rounds` rounds at most:
    /// the plan that has no step, or `None` when the last of the rounds still had one.
    fn run(&mut self, rounds: usize) -> Result<Option<SyncPlan>, Failure> {
        for _ in 0..rounds {
            self.round_count += 1;
            self.facts.places_taken_in_onedrive = self.stand_in.places_taken_in_onedrive.clone();
            let (input, plan) = plan_next_steps(&mut self.store, &self.facts, &mut self.widening);
            let given: Vec<i64> = input.items.iter().map(|item| item.local_item_id.as_i64()).collect();
            if !plan.faults.is_empty() {
                return Err(format!("the plan says it is at fault: {:?}\n  waits {:?}", plan.faults, plan.waits));
            }
            if plan.steps.is_empty() {
                return Ok(Some(plan));
            }
            let mut steps = plan.steps.clone();
            self.step_order.shuffle(&mut steps);
            for step in &steps {
                match self.stand_in.run(&mut self.store, step) {
                    StepOutcome::Written => {}
                    StepOutcome::RowsChanged => return Err(format!("the rows changed under a step of one plan: {step:?}\n  the plan's steps: {:?}\n  given: {given:?}\n  waits: {:?}", plan.steps, plan.waits)),
                    StepOutcome::NameTaken => return Err(format!("OneDrive answers that the name is taken, which the plan did not foresee: {step:?}")),
                }
                let is_listing_under_way = self.store.item_store().find_listing_under_way().unwrap().is_some();
                if let Some(rule) = find_broken_rule(&rows(&mut self.store), is_listing_under_way) {
                    return Err(format!("after {step:?}: {rule}"));
                }
            }
        }
        Ok(None)
    }

    fn run_to_the_end(&mut self) -> Result<SyncPlan, Failure> {
        self.run(ROUNDS)?.ok_or_else(|| {
            let (_, plan) = plan_next_steps(&mut self.store, &self.facts, &mut self.widening);
            format!("it does not end in {ROUNDS} rounds; the steps of the plan made then: {:?}", plan.steps)
        })
    }

    /// Whether `row` is of an item that is left here, or that the folder shows in one, or
    /// that OneDrive has where the folder cannot show it: what the plan leaves as it is.
    fn is_left_as_it_is(&mut self, by_id: &BTreeMap<LocalItemId, &Item>, row: &Item) -> bool {
        if row.onedrive.is_some() && self.store.item_store().find_folder_visibility(row.local_item_id).unwrap() == FolderVisibility::CannotBeShown {
            return true;
        }
        let mut at = Some(row.local_item_id);
        while let Some(id) = at.filter(|id| *id != ROOT) {
            if self.store.item_store().is_ignored(id).unwrap() {
                return true;
            }
            at = by_id.get(&id).and_then(|row| row.local.as_ref()).and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id);
        }
        false
    }

    /// The run has ended: the two sides are equal, nothing of `expected` was lost, and a
    /// plan over every row has no step. An item is left as it is, with whatever its sides
    /// say, when it is left here or shown in a directory that is, and when it waits for
    /// such an item: an item of OneDrive moved here into a directory that is left here
    /// waits for the user, and what waits for it waits with it.
    fn check_converged(&mut self, expected: &ExpectedEnd) -> Result<(), Failure> {
        let rows = rows(&mut self.store);
        let by_id: BTreeMap<LocalItemId, &Item> = rows.iter().map(|row| (row.local_item_id, row)).collect();
        let mut left_as_they_are = BTreeSet::new();
        for row in &rows {
            if self.is_left_as_it_is(&by_id, row) {
                left_as_they_are.insert(row.local_item_id);
            }
        }
        let plan = make_sync_plan(&describe_every_item(&mut self.store, &self.facts));
        if !plan.steps.is_empty() || !plan.faults.is_empty() {
            return Err(format!("the plan over every row of what has converged: steps {:?}, faults {:?}", plan.steps, plan.faults));
        }
        let waited_for = |item: LocalItemId| plan.waits.iter().find(|wait| wait.item == item).map(|wait| &wait.reason);
        for wait in &plan.waits {
            let (mut at, mut reason) = (wait.item, Some(&wait.reason));
            for _ in 0..plan.waits.len() {
                let Some(WaitReason::AnotherItem(another)) = reason else { break };
                (at, reason) = (*another, waited_for(*another));
            }
            if !matches!(reason, Some(WaitReason::User(Reason::Parent))) || !(left_as_they_are.contains(&at) || by_id.get(&at).is_some_and(|row| row.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| left_as_they_are.contains(&place.parent_local_item_id)))) {
                return Err(format!("the plan over every row of what has converged leaves item {} waiting: {:?}, at last item {at} for {reason:?}", wait.item, wait.reason));
            }
            left_as_they_are.insert(wait.item);
        }
        for row in rows.iter().filter(|row| !left_as_they_are.contains(&row.local_item_id)) {
            let is_no_item = row.local.is_none() && row.onedrive.is_none() && row.synced_place.is_none() && row.synced_content.is_none();
            let agrees = row.local.is_some() && row.local == row.onedrive && row.synced_place == row.local.as_ref().and_then(|local| local.place.clone()) && row.synced_content == row.local.as_ref().and_then(|local| local.content.clone());
            if !is_no_item && !agrees {
                return Err(format!("item {} does not have its three versions equal, and waits for nothing", row.local_item_id));
            }
        }
        if !self.changes.gives_ignored_names && !left_as_they_are.is_empty() {
            return Err(format!("no name is ignored, and the items {left_as_they_are:?} are left as they are"));
        }
        expected.find_loss(&rows, &left_as_they_are).map_or(Ok(()), Err)
    }

    /// Changes at random: `here` of the folder, then `in_onedrive` of OneDrive, which are
    /// answered as the changes a listing is to say.
    fn make_changes(&mut self, here: usize, in_onedrive: usize) -> Vec<Change> {
        for _ in 0..here {
            self.changes.change_folder(&mut self.store);
        }
        self.listing_count += 1;
        let mut drive = OneDriveChanges::of(&rows(&mut self.store), self.listing_count * 1_000_000);
        for _ in 0..in_onedrive {
            self.changes.change_onedrive(&mut drive);
        }
        drive.listed
    }

    /// Both sides are changed at random, a listing says OneDrive's changes, and the plan
    /// runs to its end.
    fn change_and_converge(&mut self, here: usize, in_onedrive: usize) -> Result<(), Failure> {
        let listed = self.make_changes(here, in_onedrive);
        write_listing_of_changes(&mut self.store, &listed);
        let expected = ExpectedEnd::of(&mut self.store);
        self.run_to_the_end()?;
        self.check_converged(&expected)
    }

    /// As [`change_and_converge`](Self::change_and_converge), with the listing written in
    /// two pages and the plan run to where it waits between them: the rows then say what a
    /// listing said so far.
    fn change_and_converge_through_a_listing_of_two_pages(&mut self, here: usize, in_onedrive: usize) -> Result<(), Failure> {
        let listed = self.make_changes(here, in_onedrive);
        let (first, second) = listed.split_at(listed.len() / 2);
        let listing = self.store.item_store().begin_delta_listing().unwrap();
        let outcome = self.store.item_store().write_listed_changes(listing.id, first, listing.began_at_count, &ListingLink::NextLink("next".into())).unwrap();
        assert!(outcome.is_link_written());
        // What the plan can do meanwhile, it does; what it cannot judge yet waits.
        self.run_to_the_end()?;
        // The second page is fetched now, and says OneDrive as it is now: the steps that ran
        // since the first page changed it, and the other client makes the rest of its
        // changes in what they left, each where OneDrive's rules still let it.
        self.listing_count += 1;
        let mut drive = OneDriveChanges::of(&rows(&mut self.store), self.listing_count * 1_000_000);
        second.iter().for_each(|change| drive.make_again(change));
        let fetch_began_at_count = self.store.item_store().confirmation_count().unwrap();
        let outcome = self.store.item_store().write_listed_changes(listing.id, &drive.listed, fetch_began_at_count, &ListingLink::DeltaLink("link".into())).unwrap();
        assert!(outcome.is_link_written() && outcome.changes.iter().all(|change| *change == crate::ListedChangeOutcome::Written), "{outcome:?}");
        let expected = ExpectedEnd::of(&mut self.store);
        self.run_to_the_end()?;
        self.check_converged(&expected)
    }

    /// As [`change_and_converge`](Self::change_and_converge), in a folder that is switched
    /// to read-only after the first round: what was unsent then is sent, what comes to be
    /// unsent later waits for the user, and all of it converges once the folder is
    /// read-write again.
    fn change_and_converge_through_a_read_only_folder(&mut self, here: usize, in_onedrive: usize) -> Result<(), Failure> {
        let listed = self.make_changes(here, in_onedrive);
        write_listing_of_changes(&mut self.store, &listed);
        let expected = ExpectedEnd::of(&mut self.store);
        self.run(1)?;
        self.facts.sending_state = SendingState::ReadOnly { last_unsent_since: self.store.item_store().last_unsent_since().unwrap() };
        let read_only = self.run_to_the_end()?;
        // What is left waits for the user, or for an item that does.
        let waited_for = |item: LocalItemId| read_only.waits.iter().find(|wait| wait.item == item).map(|wait| &wait.reason);
        for wait in &read_only.waits {
            let mut reason = Some(&wait.reason);
            for _ in 0..read_only.waits.len() {
                let Some(WaitReason::AnotherItem(another)) = reason else { break };
                reason = waited_for(*another);
            }
            if !matches!(reason, Some(WaitReason::User(_))) {
                return Err(format!("in the read-only folder item {} waits, and not for the user: {:?}, at last {reason:?}", wait.item, wait.reason));
            }
        }
        self.facts.sending_state = SendingState::ReadWrite;
        self.run_to_the_end()?;
        self.check_converged(&expected)
    }
}

/// What must be on both sides once a run has ended, read off the rows before it begins:
/// the model of "nothing is lost".
///
/// - *Contents.* A content a side has of a file that is not the content agreed on of that
///   file is new or changed there: it must be on both sides at the end, under some name.
///   (A content that was agreed on and that one side replaced or removed may go: that is
///   the change.)
/// - *Items.* An item the folder shows and OneDrive never had, and one OneDrive has and
///   the folder never showed, must be on both sides at the end.
///
/// Left out, by the design: what OneDrive has, and the folder does not show, below a
/// directory that was removed here — that removal is sent and stands; and what is left
/// here, which is never sent, with what the folder shows in a directory that is left here:
/// an item of OneDrive moved here into one waits for the user, with all of it.
struct ExpectedEnd {
    contents: BTreeSet<String>,
    new_here: BTreeSet<LocalItemId>,
    new_in_onedrive: Vec<OneDriveItemReference>,
}

fn content_of(version: &Option<ItemVersion>) -> Option<String> {
    version.as_ref().and_then(|version| version.content.as_ref()).map(hash_of)
}

impl ExpectedEnd {
    fn of(store: &mut TreeStore) -> Self {
        let rows = rows(store);
        let by_id: BTreeMap<LocalItemId, &Item> = rows.iter().map(|row| (row.local_item_id, row)).collect();
        let is_directory_removed_here = |row: &Item| row.kind == ItemKind::Directory && row.local.is_none() && row.onedrive.is_some() && row.synced_place.is_some();
        let goes_with_a_removal = |row: &Item| {
            let directory_of = |row: &Item| row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).and_then(|place| by_id.get(&place.parent_local_item_id).copied());
            let mut above = directory_of(row);
            let mut is_below_removed = false;
            for _ in 0..rows.len() {
                let Some(directory) = above else { break };
                is_below_removed |= is_directory_removed_here(directory);
                above = directory_of(directory);
            }
            row.local.is_none() && is_below_removed
        };
        // Left here, or shown in a directory that is: left as it is, with all of it.
        let mut is_left_as_it_is = |row: &Item| {
            let mut at = Some(row.local_item_id);
            for _ in 0..rows.len() {
                let Some(id) = at.filter(|id| *id != ROOT) else { break };
                if store.item_store().is_ignored(id).unwrap() {
                    return true;
                }
                at = by_id.get(&id).and_then(|row| row.local.as_ref()).and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id);
            }
            false
        };
        let mut expected = Self { contents: BTreeSet::new(), new_here: BTreeSet::new(), new_in_onedrive: Vec::new() };
        for row in rows.iter().filter(|row| row.local_item_id != ROOT) {
            let agreed_on = row.synced_content.as_ref().map(hash_of);
            if is_left_as_it_is(row) {
                continue;
            }
            if row.local.is_some() {
                expected.contents.extend(content_of(&row.local).filter(|content| Some(content) != agreed_on.as_ref()));
                if row.onedrive_item.is_none() {
                    expected.new_here.insert(row.local_item_id);
                }
            }
            if row.onedrive.is_some() && !goes_with_a_removal(row) {
                expected.contents.extend(content_of(&row.onedrive).filter(|content| Some(content) != agreed_on.as_ref()));
                if row.local.is_none() && row.synced_place.is_none() && row.synced_content.is_none() {
                    expected.new_in_onedrive.extend(row.onedrive_item.clone());
                }
            }
        }
        expected
    }

    /// What of this `rows`, those of the end of a run, lack; `None` when nothing was lost.
    /// `left_as_they_are` are the items that are left as they are at the end: what one of
    /// them has on a side is not asked to be on the other.
    fn find_loss(&self, rows: &[Item], left_as_they_are: &BTreeSet<LocalItemId>) -> Option<Failure> {
        let here: BTreeSet<String> = rows.iter().filter_map(|row| content_of(&row.local)).collect();
        let in_onedrive: BTreeSet<String> = rows.iter().filter_map(|row| content_of(&row.onedrive)).collect();
        let left: Vec<&Item> = rows.iter().filter(|row| left_as_they_are.contains(&row.local_item_id)).collect();
        let is_left_here = |content: &String| left.iter().any(|row| content_of(&row.local).as_ref() == Some(content));
        let is_left_in_onedrive = |content: &String| left.iter().any(|row| content_of(&row.onedrive).as_ref() == Some(content));
        for content in &self.contents {
            if !here.contains(content) && !is_left_in_onedrive(content) {
                return Some(format!("the content {content}, which one side alone had, is not in the folder"));
            }
            if !in_onedrive.contains(content) && !is_left_here(content) {
                return Some(format!("the content {content}, which one side alone had, is not in OneDrive"));
            }
        }
        for new in &self.new_here {
            let is_on_both_sides = rows.iter().any(|row| row.local_item_id == *new && ((row.local.is_some() && row.onedrive.is_some()) || left_as_they_are.contains(new)));
            if !is_on_both_sides {
                return Some(format!("item {new}, which the folder alone had, is not on both sides"));
            }
        }
        for new in &self.new_in_onedrive {
            let is_on_both_sides = rows.iter().any(|row| row.onedrive_item.as_ref() == Some(new) && row.onedrive.is_some() && (row.local.is_some() || left_as_they_are.contains(&row.local_item_id)));
            if !is_on_both_sides {
                return Some(format!("the item {} of OneDrive, which OneDrive alone had, is not on both sides", new.item_id.as_str()));
            }
        }
        None
    }
}

/// How a run changes the two sides and converges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    /// One listing of one page, a read-write folder, every row read at once.
    Plain,
    /// The listing in two pages, the plan running between them.
    ListingOfTwoPages,
    /// The folder switched to read-only after the first round, and back at the end.
    ReadOnlyFolder,
    /// Two unsent items and two OneDrive changed are read at first, and the directories
    /// above an item are given only when the plan asks for them.
    FewItemsGiven,
}

/// One run: a store from `seed`, changed and converged `times` times, `changes` changes on
/// each side each time. What failed, with the changes that were made, in words.
fn run(seed: u64, kind: RunKind, times: usize, changes: usize) -> Result<World, Failure> {
    let mut world = World::new(seed);
    // Every other seed gives no ignored name and no item the folder cannot show: then
    // nothing at all is left as it is.
    world.changes.gives_ignored_names = seed % 2 == 0;
    world.changes.makes_items_the_folder_cannot_show = seed % 2 == 0;
    if kind == RunKind::FewItemsGiven {
        world.facts.items_read_at_once = 2;
        world.facts.gives_directories_above = false;
    }
    for time in 0..times {
        let said_before = world.changes.said.len();
        let converged = match kind {
            RunKind::Plain | RunKind::FewItemsGiven => world.change_and_converge(changes, changes),
            RunKind::ListingOfTwoPages => world.change_and_converge_through_a_listing_of_two_pages(changes, changes),
            RunKind::ReadOnlyFolder => world.change_and_converge_through_a_read_only_folder(changes, changes),
        };
        if let Err(failure) = converged {
            let said = world.changes.said[said_before..].join("\n  ");
            return Err(format!("seed {seed}, {kind:?}, {changes} changes a side, time {time}: {failure}\n the changes:\n  {said}\n the rows:\n{}", describe_rows(&rows(&mut world.store))));
        }
    }
    Ok(world)
}

const RUN_KINDS: [RunKind; 4] = [RunKind::Plain, RunKind::ListingOfTwoPages, RunKind::ReadOnlyFolder, RunKind::FewItemsGiven];

/// The design's §12, "convergence": new. A few seeds of each kind of run.
#[test]
fn changes_made_at_random_on_both_sides_converge_and_nothing_is_lost() {
    for kind in RUN_KINDS {
        for seed in 1..=8 {
            run(seed, kind, 4, 6).unwrap_or_else(|failure| panic!("{failure}"));
        }
    }
}

/// One run by hand, to read a failure of the larger run:
/// `CONVERGENCE_RUN=<seed>,<kind 0 to 3>,<changes> cargo test -p konedrive-tree one_run_by_hand -- --ignored --nocapture`.
#[test]
#[ignore = "one run of the convergence test, named by the environment"]
fn one_run_by_hand() {
    let named = std::env::var("CONVERGENCE_RUN").expect("CONVERGENCE_RUN=<seed>,<kind>,<changes>");
    let numbers: Vec<u64> = named.split(',').map(|number| number.parse().expect("a number")).collect();
    match run(numbers[0], RUN_KINDS[numbers[1] as usize], 5, numbers[2] as usize) {
        Ok(world) => println!("converged in {} rounds", world.round_count),
        Err(failure) => panic!("{failure}"),
    }
}

/// The larger run, by hand: `cargo test -p konedrive-tree many_seeds -- --ignored
/// --nocapture`. Every failure is said with the fewest changes a side that still fail for
/// its seed, the smallest first.
#[test]
#[ignore = "the larger run of the convergence test: minutes"]
fn many_seeds_of_changes_made_at_random_converge_and_nothing_is_lost() {
    let mut failures: Vec<(usize, Failure)> = Vec::new();
    let (mut runs, mut rounds, mut widening) = (0, 0, WideningCount::default());
    for kind in RUN_KINDS {
        for seed in 1..=400 {
            let ran = std::panic::catch_unwind(|| run(seed, kind, 5, 7).map(|world| (world.round_count, world.widening)));
            runs += 1;
            match ran {
                Ok(Ok((round_count, widened))) => {
                    rounds += round_count;
                    widening.items_asked_for += widened.items_asked_for;
                    widening.readings += widened.readings;
                }
                Ok(Err(_)) | Err(_) => {
                    let smallest = (1..=7).find_map(|changes| match std::panic::catch_unwind(|| run(seed, kind, 5, changes).map(|_| ())) {
                        Ok(Ok(())) => None,
                        Ok(Err(failure)) => Some((changes, failure)),
                        Err(_) => Some((changes, format!("seed {seed}, {kind:?}, {changes} changes a side: panicked"))),
                    });
                    failures.extend(smallest);
                }
            }
        }
    }
    failures.sort_by_key(|(changes, failure)| (*changes, failure.len()));
    // One failure of each kind, the smallest: a kind is what failed, with no numbers.
    let kind_of = |failure: &str| failure.lines().next().unwrap_or_default().split_once(": ").map_or(String::new(), |(_, what)| what.chars().filter(|letter| !letter.is_ascii_digit()).take(70).collect::<String>());
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for (_, failure) in &failures {
        let count = kinds.entry(kind_of(failure)).or_default();
        *count += 1;
        if *count == 1 {
            println!("{failure}\n");
        }
    }
    println!("{kinds:#?}");
    println!("{runs} runs, {rounds} rounds in those that converged, {} items the plan asked for, {} wider readings, {} failures", widening.items_asked_for, widening.readings, failures.len());
    assert!(failures.is_empty(), "{} of {runs} runs failed", failures.len());
}

// -- circles, made on purpose

/// A world with the files `names` in the root, agreed on: their local item ids.
fn world_with_files(names: &[&str]) -> (World, Vec<LocalItemId>) {
    let mut world = World::new(1);
    let listed: Vec<Change> = names.iter().enumerate().map(|(number, name)| listed_file(&format!("C{number}"), "R", name, (500 + number as u64, &format!("h{}-1000", 500 + number)), "e")).collect();
    write_listing_of_changes(&mut world.store, &listed);
    let expected = ExpectedEnd::of(&mut world.store);
    world.run_to_the_end().and_then(|_| world.check_converged(&expected)).unwrap_or_else(|failure| panic!("{failure}"));
    let ids = names.iter().map(|name| world.store.item_store().find_item_at(ROOT, name.as_ref()).unwrap().expect("shown").local_item_id).collect();
    (world, ids)
}

/// The names of `items` here go round by one: each has the name of the next, the last that
/// of the first.
fn rotate_names_here(world: &mut World, items: &[LocalItemId]) {
    for pair in items.windows(2) {
        world.store.item_store().exchange_local_places(pair[0], pair[1]).unwrap();
    }
}

/// The names of the files `C<first>` … `C<last>` in OneDrive go round by `by`, as a listing
/// says it: each has the name the file `by` after it had.
fn rotate_names_in_onedrive(names: &[&str], numbers: std::ops::Range<usize>, by: usize) -> Vec<Change> {
    let count = numbers.len();
    numbers.clone().enumerate().map(|(at, number)| listed_file(&format!("C{number}"), "R", names[numbers.start + (at + by) % count], (500 + number as u64, &format!("h{}-1000", 500 + number)), "e-rotated")).collect()
}

fn names_here(world: &mut World, items: &[LocalItemId]) -> Vec<String> {
    items.iter().map(|item| world.store.item_store().find_item(*item).unwrap().unwrap().local.unwrap().place.unwrap().name.to_string_lossy().into_owned()).collect()
}

/// Stage B proved a circle of two and of three names; here four and five go round, on
/// either side, and three circles at once, one of them on both sides.
#[test]
fn circles_of_four_and_more_names_and_several_circles_at_once_converge() {
    let names = ["n0", "n1", "n2", "n3", "n4", "m0", "m1", "m2", "m3", "k0", "k1", "k2", "k3"];
    let (mut world, ids) = world_with_files(&names);
    // Five names go round here; four in OneDrive; and four on both sides, the other way.
    rotate_names_here(&mut world, &ids[0..5]);
    let mut listed = rotate_names_in_onedrive(&names, 5..9, 1);
    rotate_names_here(&mut world, &ids[9..13]);
    listed.extend(rotate_names_in_onedrive(&names, 9..13, 3));
    write_listing_of_changes(&mut world.store, &listed);
    let expected = ExpectedEnd::of(&mut world.store);
    world.run_to_the_end().and_then(|_| world.check_converged(&expected)).unwrap_or_else(|failure| panic!("{failure}\n{}", describe_rows(&rows(&mut world.store))));
    assert_eq!(names_here(&mut world, &ids[0..5]), ["n1", "n2", "n3", "n4", "n0"], "what was done here stands");
    assert_eq!(names_here(&mut world, &ids[5..9]), ["m1", "m2", "m3", "m0"], "what OneDrive did stands");
}

/// Directories moved into one another on the two sides, four in a ring: each was moved
/// into the next, two of the moves here and two in OneDrive. OneDrive's moves stand, and
/// the moves here that would close the ring are taken back.
#[test]
fn directories_moved_into_one_another_in_a_ring_of_four_converge() {
    let mut world = World::new(1);
    let listed: Vec<Change> = (0..4).map(|number| listed_directory(&format!("G{number}"), "R", &format!("ring{number}"), "e")).collect();
    write_listing_of_changes(&mut world.store, &listed);
    world.run_to_the_end().unwrap_or_else(|failure| panic!("{failure}"));
    let ids: Vec<LocalItemId> = (0..4).map(|number| world.store.item_store().find_item_at(ROOT, format!("ring{number}").as_ref()).unwrap().expect("shown").local_item_id).collect();
    for number in [0, 2] {
        world.store.item_store().apply(&LocalVersionChange::Moved { item: ids[number], place: place(ids[number + 1], &format!("ring{number}")), is_name_ignored: false }).unwrap();
    }
    let listed: Vec<Change> = [1, 3].iter().map(|number| listed_directory(&format!("G{number}"), &format!("G{}", (number + 1) % 4), &format!("ring{number}"), "e-moved")).collect();
    write_listing_of_changes(&mut world.store, &listed);
    let expected = ExpectedEnd::of(&mut world.store);
    world.run_to_the_end().and_then(|_| world.check_converged(&expected)).unwrap_or_else(|failure| panic!("{failure}\n{}", describe_rows(&rows(&mut world.store))));
}
