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
//! 3. at the end every item has its three versions equal but for the time of OneDrive's
//!    content, which the content here need not have, but an item that is left here and
//!    what is in one;
//! 4. nothing is lost ([`ExpectedEnd`]): every content that one side alone had, new or
//!    changed there, is on both sides under some name; every item one side alone had is on
//!    both sides; every item both sides had, that neither removed, still is; and what the
//!    user made is in the folder whatever became of it in OneDrive. No removal of a
//!    directory is such a loss: what OneDrive made in a directory that was removed here,
//!    and the content OneDrive gave since to a file the user removed in it, the folder
//!    never showed, and both are on both sides at the end;
//! 5. a plan over every row of what has converged has no step;
//! 6. no step leaves that must not ([`step_rules`]): asked of every step of every run, in
//!    words of its own, beside the plan's and the commits';
//! 7. where OneDrive is a state of its own ([`requests_in_flight`]), the rows say at the
//!    end what it has.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use super::*;
use crate::item_store::tests::listed_drive::{listed_directory, listed_file, reference as onedrive_reference, store_with_listed_root, write_listing_of_changes};
use crate::item_store::tests::row_rules::find_broken_rule;
use crate::model::{Change, ContentPresence, Kind, LocalVersionChange};
use crate::{ListingLink, TreeStore};

/// OneDrive in these tests: what it has, and what changes it.
mod drive;
/// The caller of the plan in miniature: what it reads and gives.
pub(crate) mod plan_input;
/// Requests that are answered late or never, and pages written after what they are older
/// than.
mod requests_in_flight;
/// What a step must never be, said apart from the code that plans and commits it.
mod step_rules;
/// Changes made at random on both sides, from a seed.
mod random_changes;
/// The stand-in for OneDrive and the folder: the commits of the steps.
mod step_stand_in;
/// The sequences of the reviews, as the reviews ran them, with the state each ends in.
mod review_sequences;

use plan_input::{describe_every_item, plan_next_steps, CallerFacts, WideningCount};
use drive::OneDriveChanges;
use random_changes::{RandomChanges, RandomSource};
use requests_in_flight::RequestsInFlight;
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
    /// For how many rounds more the caller's facts are drawn at random, and steps are left
    /// running ([`RunKind::CallerFactsDrawn`]).
    fact_rounds_left: usize,
    /// The steps that are running: each with the number of rounds after which its commit
    /// arrives, with the versions it was planned from.
    running_steps: Vec<(SyncStep, usize)>,
    what_facts_are_drawn_with: RandomSource,
    /// What must be on both sides, read after the folder was last changed while steps ran.
    expected_since_steps_ran: Option<ExpectedEnd>,
    /// OneDrive as a state of its own ([`RunKind::RequestsInFlight`]): the requests of the
    /// steps change it whatever becomes of their commits, and a listing reads it. `None`
    /// where OneDrive is what the rows say it has.
    drive: Option<OneDriveChanges>,
    requests: RequestsInFlight,
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
            fact_rounds_left: 0,
            running_steps: Vec::new(),
            what_facts_are_drawn_with: RandomSource::new(seed ^ 0xFAC7),
            expected_since_steps_ran: None,
            drive: None,
            requests: RequestsInFlight::default(),
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

    /// The directory `step` sends its item into, when it sends one into a directory.
    fn destination_of(store: &mut TreeStore, step: &SyncStep) -> Option<LocalItemId> {
        match step {
            SyncStep::OneDrive(OneDriveStep::MakeDirectory { parent, .. } | OneDriveStep::SendNewFile { parent, .. } | OneDriveStep::SendMove { parent, .. }) => {
                store.item_store().find_bound_item(parent).unwrap().map(|directory| directory.local_item_id)
            }
            _ => None,
        }
    }

    /// The commits of the steps that were left running arrive, each after its rounds: with
    /// the versions the step was planned from, whatever became of the rows meanwhile. A
    /// commit that finds the rows changed, or a name taken, is no failure here: that is
    /// what a step that ran for a while is answered.
    fn let_running_steps_end(&mut self) -> Result<(), Failure> {
        let (ended, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.running_steps).into_iter().partition(|(_, rounds_left)| *rounds_left == 0);
        self.running_steps = running.into_iter().map(|(step, rounds_left)| (step, rounds_left - 1)).collect();
        for (step, _) in ended {
            self.run_step(&step)?;
            self.facts.items_with_running_step.remove(&step.local_item_id());
            self.facts.destinations_of_running_steps = self.running_steps.iter().filter_map(|(step, _)| Self::destination_of(&mut self.store, step)).collect();
            if let Some(rule) = find_broken_rule(&rows(&mut self.store), self.store.item_store().find_listing_under_way().unwrap().is_some()) {
                return Err(format!("after {step:?}, which ran for a while: {rule}"));
            }
        }
        Ok(())
    }

    /// What only the caller knows, drawn at random for one round: files a program holds
    /// open, files in their quiet time, OneDrive full. Nothing of it
    /// once the rounds of drawn facts are over.
    fn draw_callers_facts(&mut self) {
        let is_drawn = self.fact_rounds_left > 0;
        self.fact_rounds_left = self.fact_rounds_left.saturating_sub(1);
        let files: Vec<LocalItemId> = rows(&mut self.store).iter().filter(|row| row.kind == ItemKind::File && row.local.is_some()).map(|row| row.local_item_id).collect();
        let random = &mut self.what_facts_are_drawn_with;
        self.facts.items_open_for_writing = files.iter().copied().filter(|_| is_drawn && random.below(6) == 0).collect();
        self.facts.quiet_until = files.iter().copied().filter(|_| is_drawn && random.below(6) == 0).map(|file| (file, 1000)).collect();
        self.facts.is_onedrive_full = is_drawn && random.below(4) == 0;
    }

    /// Makes plans and runs their steps until a plan has none, for `rounds` rounds at most:
    /// the plan that has no step, or `None` when the last of the rounds still had one.
    /// While the caller's facts are drawn ([`RunKind::CallerFactsDrawn`]), some of the
    /// steps towards OneDrive and towards the folder are left running for a round or two,
    /// and the folder is changed while they run.
    fn run(&mut self, rounds: usize) -> Result<Option<SyncPlan>, Failure> {
        for _ in 0..rounds {
            self.round_count += 1;
            self.let_running_steps_end()?;
            let leaves_steps_running = self.fact_rounds_left > 0;
            self.draw_callers_facts();
            self.facts.places_taken_in_onedrive = self.stand_in.places_taken_in_onedrive.clone();
            let (input, plan) = plan_next_steps(&mut self.store, &self.facts, &mut self.widening);
            let given: Vec<i64> = input.items.iter().map(|item| item.local_item_id.as_i64()).collect();
            if !plan.faults.is_empty() {
                return Err(format!("the plan says it is at fault: {:?}\n  waits {:?}", plan.faults, plan.waits));
            }
            if plan.steps.is_empty() {
                if leaves_steps_running || !self.running_steps.is_empty() {
                    continue;
                }
                return Ok(Some(plan));
            }
            let mut steps = plan.steps.clone();
            self.step_order.shuffle(&mut steps);
            let (mut is_left_running, mut is_refused) = (false, false);
            for step in &steps {
                if let Some(refused) = step_rules::find_step_that_must_not_leave(&mut self.store, &self.facts, self.drive.as_ref(), step) {
                    return Err(format!("{refused}\n  the plan's steps: {:?}", plan.steps));
                }
                if leaves_steps_running && !matches!(step, SyncStep::Row(_)) && self.what_facts_are_drawn_with.below(3) == 0 {
                    self.running_steps.push((step.clone(), self.what_facts_are_drawn_with.below(2)));
                    self.facts.items_with_running_step.insert(step.local_item_id());
                    let destination = Self::destination_of(&mut self.store, step);
                    self.facts.destinations_of_running_steps.extend(destination);
                    is_left_running = true;
                    continue;
                }
                match self.run_step(step)? {
                    StepOutcome::Written => {}
                    // OneDrive is a state of its own, and has not what the rows say: the
                    // items the request named are read again before the next plan.
                    StepOutcome::NotDone if self.drive.is_some() => is_refused = true,
                    StepOutcome::NotDone => return Err(format!("OneDrive refused a step of one plan: {step:?}")),
                    StepOutcome::RowsChanged => return Err(format!("the rows changed under a step of one plan: {step:?}\n  the plan's steps: {:?}\n  given: {given:?}\n  waits: {:?}", plan.steps, plan.waits)),
                    StepOutcome::NameTaken if !self.stand_in.places_taken_in_onedrive.is_empty() && self.expected_since_steps_ran.is_some() => {}
                    StepOutcome::NameTaken => return Err(format!("OneDrive answers that the name is taken, which the plan did not foresee: {step:?}")),
                }
                let is_listing_under_way = self.store.item_store().find_listing_under_way().unwrap().is_some();
                if let Some(rule) = find_broken_rule(&rows(&mut self.store), is_listing_under_way) {
                    return Err(format!("after {step:?}: {rule}"));
                }
            }
            if is_refused {
                self.list_drive_changes()?;
            }
            if is_left_running {
                // The user goes on working while the steps run.
                for _ in 0..2 {
                    self.changes.change_folder(&mut self.store);
                }
                let mut expected = ExpectedEnd::of(&mut self.store);
                // A file removed here while a step runs for it: where OneDrive has it is
                // known once that step ended — its move into a directory that was removed
                // here since may yet be confirmed, and its content then goes with that
                // directory's removal, which stands.
                for row in rows(&mut self.store).iter().filter(|row| row.local.is_none() && self.facts.items_with_running_step.contains(&row.local_item_id)) {
                    expected.contents.retain(|content| Some(content) != content_of(&row.onedrive).as_ref());
                }
                self.expected_since_steps_ran = Some(expected);
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
            // The time of a content here stays local: OneDrive's content is the one agreed
            // on but for its time, where the two were agreed on without a file written.
            let (place, content) = (|version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.place.clone()), |version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.content.clone()));
            let agrees = row.local.is_some()
                && row.onedrive.is_some()
                && place(&row.local) == place(&row.onedrive)
                && row.synced_place == place(&row.local)
                && row.synced_content == content(&row.local)
                && crate::sync_plan::side_change::is_same_content_but_for_time(content(&row.onedrive).as_ref(), row.synced_content.as_ref());
            if !is_no_item && !agrees {
                return Err(format!("item {} does not have its three versions equal but for the time of OneDrive's content, and waits for nothing", row.local_item_id));
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
        self.changes.make_case_on_purpose(&mut self.store, &mut drive);
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
        self.expected_since_steps_ran = None;
        self.run_to_the_end()?;
        let expected = self.expected_since_steps_ran.take().unwrap_or(expected);
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
        // What the plan can do meanwhile, it does; what it cannot judge yet waits, for the
        // listing, for the user, or for an item that does.
        let waiting = self.run_to_the_end()?;
        let waited_for = |item: LocalItemId| waiting.waits.iter().find(|wait| wait.item == item).map(|wait| &wait.reason);
        for wait in &waiting.waits {
            let mut reason = Some(&wait.reason);
            for _ in 0..waiting.waits.len() {
                let Some(WaitReason::AnotherItem(another)) = reason else { break };
                reason = waited_for(*another);
            }
            if !matches!(reason, Some(WaitReason::Listing | WaitReason::User(_))) {
                return Err(format!("while a listing is under way item {} waits for nothing that ends: {:?}, at last {reason:?}", wait.item, wait.reason));
            }
        }
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

}

/// What must be on both sides once a run has ended, read off the rows before it begins:
/// the model of "nothing is lost".
///
/// - *Contents.* A content a side has of a file that is not the content agreed on of that
///   file is new or changed there: it must be on both sides at the end, under some name.
///   (A content that was agreed on and that one side replaced or removed may go: that is
///   the change.)
/// - *Items.* An item the folder shows and OneDrive never had, and one OneDrive has and
///   the folder never showed, must be on both sides at the end. What the user made must be
///   in the folder at the end whatever else: also where it waits for the user for good, in
///   a directory the folder cannot show as OneDrive has it.
/// - *What both sides have* and neither removed must be on both sides at the end: the
///   plan's first duty. Not asked of an item in a directory OneDrive removed, which goes
///   with it when it is clean, nor of one the folder cannot show.
/// - *What OneDrive removed and the folder changed*: a file whose content was changed here,
///   or that was moved here with its content on disk, is sent again, and is on both sides
///   at the end.
///
/// Not left out: what OneDrive has below a directory that was removed here and that the
/// folder never showed — an item, or the content OneDrive gave since to a file the user
/// removed —, which no removal takes. Left out: what is left here, which is never sent,
/// with what the folder shows in a directory that is left here: an item of OneDrive moved
/// here into one waits for the user, with all of it.
struct ExpectedEnd {
    contents: BTreeSet<String>,
    /// The contents of the files the folder alone has: of what the user made.
    contents_of_new_items: BTreeSet<String>,
    new_here: BTreeSet<LocalItemId>,
    new_in_onedrive: Vec<OneDriveItemReference>,
    on_both_sides: BTreeSet<LocalItemId>,
    sent_again: BTreeSet<LocalItemId>,
}

fn content_of(version: &Option<ItemVersion>) -> Option<String> {
    version.as_ref().and_then(|version| version.content.as_ref()).map(hash_of)
}

impl ExpectedEnd {
    fn of(store: &mut TreeStore) -> Self {
        let rows = rows(store);
        let by_id: BTreeMap<LocalItemId, &Item> = rows.iter().map(|row| (row.local_item_id, row)).collect();
        let cannot_be_shown: BTreeSet<LocalItemId> =
            rows.iter().filter(|row| row.onedrive.is_some() && store.item_store().find_folder_visibility(row.local_item_id).unwrap() != FolderVisibility::CanBeShown).map(|row| row.local_item_id).collect();
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
        // Shown in a directory OneDrive removed, at any depth: it goes with it when clean.
        let is_in_directory_onedrive_removed = |row: &Item| {
            let directory_of = |row: &Item| row.local.as_ref().and_then(|local| local.place.as_ref()).and_then(|place| by_id.get(&place.parent_local_item_id).copied());
            let mut above = directory_of(row);
            for _ in 0..rows.len() {
                let Some(directory) = above else { break };
                if directory.onedrive.is_none() && directory.local_item_id != ROOT {
                    return true;
                }
                above = directory_of(directory);
            }
            false
        };
        let mut expected = Self { contents: BTreeSet::new(), contents_of_new_items: BTreeSet::new(), new_here: BTreeSet::new(), new_in_onedrive: Vec::new(), on_both_sides: BTreeSet::new(), sent_again: BTreeSet::new() };
        for row in rows.iter().filter(|row| row.local_item_id != ROOT) {
            let agreed_on = row.synced_content.as_ref().map(hash_of);
            if is_left_as_it_is(row) {
                continue;
            }
            if row.local.is_some() {
                expected.contents.extend(content_of(&row.local).filter(|content| Some(content) != agreed_on.as_ref()));
                if row.onedrive_item.is_none() {
                    expected.new_here.insert(row.local_item_id);
                    expected.contents_of_new_items.extend(content_of(&row.local));
                }
                if row.onedrive.is_some() && !cannot_be_shown.contains(&row.local_item_id) && !is_in_directory_onedrive_removed(row) {
                    expected.on_both_sides.insert(row.local_item_id);
                }
                let is_changed_here = content_of(&row.local) != agreed_on;
                let is_moved_here_on_disk = row.local.as_ref().and_then(|local| local.place.as_ref()) != row.synced_place.as_ref() && row.content_presence == ContentPresence::OnDisk;
                if row.kind == ItemKind::File && row.onedrive.is_none() && row.synced_place.is_some() && (is_changed_here || is_moved_here_on_disk) {
                    expected.sent_again.insert(row.local_item_id);
                }
            }
            if row.onedrive.is_some() {
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
        // What is left here for good is never sent; an item OneDrive has that waits for the
        // user with its place has its content sent all the same.
        let is_left_here = |content: &String| left.iter().any(|row| row.onedrive_item.is_none() && content_of(&row.local).as_ref() == Some(content));
        let is_left_in_onedrive = |content: &String| left.iter().any(|row| content_of(&row.onedrive).as_ref() == Some(content));
        for content in &self.contents {
            if !here.contains(content) && !is_left_in_onedrive(content) {
                return Some(format!("the content {content}, which one side alone had, is not in the folder"));
            }
            if !in_onedrive.contains(content) && !is_left_here(content) {
                return Some(format!("the content {content}, which one side alone had, is not in OneDrive"));
            }
        }
        for content in &self.contents_of_new_items {
            if !here.contains(content) {
                return Some(format!("the content {content} of what the user made is not in the folder"));
            }
        }
        for new in &self.new_here {
            let is_on_both_sides = rows.iter().any(|row| row.local_item_id == *new && row.local.is_some() && (row.onedrive.is_some() || left_as_they_are.contains(new)));
            if !is_on_both_sides {
                return Some(format!("item {new}, which the folder alone had, is not on both sides, or not in the folder"));
            }
        }
        for (items, what) in [(&self.on_both_sides, "which both sides had and neither removed"), (&self.sent_again, "which OneDrive removed and the folder changed or moved")] {
            for item in items {
                let is_on_both_sides = rows.iter().any(|row| row.local_item_id == *item && row.local.is_some() && (row.onedrive.is_some() || left_as_they_are.contains(item)));
                if !is_on_both_sides {
                    return Some(format!("item {item}, {what}, is not on both sides"));
                }
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
    /// Two unsent items and two OneDrive changed are read at first, and the directories
    /// above an item are given only when the plan asks for them.
    FewItemsGiven,
    /// For four rounds what only the caller knows is drawn at random — files held open for
    /// writing, files in their quiet time, OneDrive full, removals not confirmed — and
    /// steps are left running for a round or two while the folder is changed again.
    CallerFactsDrawn,
    /// OneDrive is a state of its own ([`requests_in_flight`]): requests are answered late
    /// or never, pages are fetched before a confirmation and written after it, and both
    /// sides go on changing what the requests are about.
    RequestsInFlight,
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
    if kind == RunKind::RequestsInFlight {
        world.drive = Some(OneDriveChanges::of(&rows(&mut world.store), 1_000_000_000));
    }
    for time in 0..times {
        let said_before = world.changes.said.len();
        world.fact_rounds_left = if kind == RunKind::CallerFactsDrawn { 4 } else { 0 };
        let converged = match kind {
            RunKind::Plain | RunKind::FewItemsGiven | RunKind::CallerFactsDrawn => world.change_and_converge(changes, changes),
            RunKind::ListingOfTwoPages => world.change_and_converge_through_a_listing_of_two_pages(changes, changes),
            RunKind::RequestsInFlight => world.change_and_converge_with_requests_in_flight(changes, changes),
        };
        if let Err(failure) = converged {
            let said = world.changes.said[said_before..].join("\n  ");
            return Err(format!("seed {seed}, {kind:?}, {changes} changes a side, time {time}: {failure}\n the changes:\n  {said}\n the rows:\n{}", describe_rows(&rows(&mut world.store))));
        }
    }
    Ok(world)
}

const RUN_KINDS: [RunKind; 5] = [RunKind::Plain, RunKind::ListingOfTwoPages, RunKind::FewItemsGiven, RunKind::CallerFactsDrawn, RunKind::RequestsInFlight];

/// The design's §12, "convergence": new. A few seeds of each kind of run; more of the one
/// with requests in flight, whose cases are rarer.
#[test]
fn changes_made_at_random_on_both_sides_converge_and_nothing_is_lost() {
    for kind in RUN_KINDS {
        for seed in 1..=if kind == RunKind::RequestsInFlight { 12 } else { 8 } {
            run(seed, kind, 4, 6).unwrap_or_else(|failure| panic!("{failure}"));
        }
    }
}

/// One run by hand, to read a failure of the larger run:
/// `CONVERGENCE_RUN=<seed>,<kind 0 to 5>,<changes> cargo test -p konedrive-tree one_run_by_hand -- --ignored --nocapture`.
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

// -- the findings of the review of part 3, as the review ran them

/// The review's M3: a listing that has ended names two files in one directory under names
/// that fold to one. Both are shown, and nothing asks for a listing again.
#[test]
fn two_files_onedrive_lists_under_names_that_fold_to_one_are_both_shown() {
    let mut world = World::new(1);
    let listed = [listed_file("U1", "R", "\u{e9}clair", (701, "h701-1000"), "e"), listed_file("U2", "R", "e\u{301}clair", (702, "h702-1000"), "e")];
    write_listing_of_changes(&mut world.store, &listed);
    let expected = ExpectedEnd::of(&mut world.store);
    world.run_to_the_end().and_then(|_| world.check_converged(&expected)).unwrap_or_else(|failure| panic!("{failure}"));
    for name in ["\u{e9}clair", "e\u{301}clair"] {
        assert!(world.store.item_store().find_item_at(ROOT, name.as_ref()).unwrap().is_some(), "{name}");
    }
}

/// The review's M1, its sequence: `a` is renamed here and its move is running; meanwhile a
/// file is moved out of `a/b`, and `a` is removed with all in it. Whichever order the steps
/// run in, the file that was moved out is on both sides at the end.
#[test]
fn a_file_moved_out_of_a_removed_directory_survives_the_removal_while_a_step_runs_for_the_outer_one() {
    for seed in 1..=6 {
        let mut world = World::new(seed);
        let find = |world: &mut World, parent: LocalItemId, name: &str| world.store.item_store().find_item_at(parent, name.as_ref()).unwrap().expect("shown").local_item_id;
        let a = find(&mut world, ROOT, "a");
        let b = find(&mut world, a, "b");
        let kept = find(&mut world, b, "c.txt");
        world.store.item_store().apply(&LocalVersionChange::Moved { item: a, place: place(ROOT, "a2"), is_name_ignored: false }).unwrap();
        let (_, plan) = plan_next_steps(&mut world.store, &world.facts, &mut world.widening);
        let [move_of_a] = plan.steps.as_slice() else { panic!("{:?}", plan.steps) };
        world.running_steps.push((move_of_a.clone(), 1));
        world.facts.items_with_running_step.insert(a);
        let inside: Vec<LocalItemId> = ["c.txt", "e.txt"].iter().map(|name| find(&mut world, a, name)).collect();
        world.store.item_store().apply(&LocalVersionChange::Moved { item: kept, place: place(ROOT, "kept.txt"), is_name_ignored: false }).unwrap();
        for removed in inside.into_iter().chain([b, a]) {
            world.store.item_store().apply(&LocalVersionChange::Removed { item: removed }).unwrap();
        }
        let expected = ExpectedEnd::of(&mut world.store);
        assert!(expected.on_both_sides.contains(&kept));
        world.run_to_the_end().and_then(|_| world.check_converged(&expected)).unwrap_or_else(|failure| panic!("seed {seed}: {failure}\n{}", describe_rows(&rows(&mut world.store))));
        let kept = world.store.item_store().find_item(kept).unwrap().expect("its row");
        assert_eq!(kept.local.as_ref().and_then(|local| local.place.clone()), Some(place(ROOT, "kept.txt")));
        assert!(kept.onedrive.is_some() && world.store.item_store().find_item_at(ROOT, "a2".as_ref()).unwrap().is_none());
    }
}
