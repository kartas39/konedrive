//! What a plan is made from: read from the store by its bounded readers, closed as
//! `SyncPlanInput` says, described, planned, and widened by every item the plan answers
//! it was not given. The production counterpart of the convergence test's caller
//! (`konedrive-tree`'s `sync_plan/tests/convergence/plan_input.rs`), which is its
//! reference line by line.
//!
//! **State owned:** none. Everything here runs inside ONE read closure of the store's
//! reading connection (`Store::read_items`), so one read transaction: the rows, the step
//! memory and the listing's state a plan is made from are of one moment, and no job of
//! the writer waits for it.
//!
//! **Answers for:** the map's MISSING-2.h (the closing of the set) and the gathering of
//! `SyncPlanInput` (P5): design §6, "what it is given" — among it what the steps remember
//! (`next_tries`, `sending_refusals`), what the task itself remembers of the steps towards
//! the folder and of the rows that failed (`next_tries` too) and what the check at start left
//! (`items_left_by_start_up_check`), which the plan is TOLD and answers as its own waits.
//!
//! **Tested without the rest**, over a store in memory:
// test: the_unsent_and_the_changed_in_onedrive_are_read_bounded
// test: the_pinned_to_download_are_read_with_every_plan
// test: an_item_is_given_with_the_directories_above_it_on_every_side
// test: an_item_that_differs_is_given_with_the_holders_of_its_name_its_copy_names_and_its_temporary_name
// test: a_directory_removed_here_is_given_with_all_onedrive_has_in_it_and_their_count
// test: a_shown_directory_that_cannot_be_shown_is_given_with_what_the_folder_shows_in_it_and_their_count
// test: an_item_the_plan_answers_it_was_not_given_is_given_and_the_plan_made_again
// test: a_directory_whose_items_the_plan_answers_were_not_given_is_given_them_with_their_count
// test: twice_as_many_are_read_when_no_step_can_run_and_more_is_there
// test: a_window_whose_every_item_is_refused_or_waits_for_its_next_try_is_widened_until_a_step_can_run
// test: the_items_open_for_writing_are_those_whose_row_keeps_a_content_before_the_write
// test: the_confirmed_removals_and_the_taken_names_come_from_the_step_memory
// test: an_item_whose_next_try_is_kept_is_given_with_it_and_its_reason
// test: the_tasks_tries_whose_rows_changed_or_went_are_void_before_the_plan_is_told
// test: a_step_towards_the_folder_that_failed_waits_until_its_next_try_and_a_sending_of_the_item_does_not
// test: an_item_refused_for_good_or_too_big_for_the_space_is_given_as_refused_with_the_reason
// test: the_items_the_check_left_are_given_as_left_by_it
// test: the_rows_of_the_directories_running_steps_send_into_are_given_as_destinations
// test: the_removals_sent_a_short_time_ago_are_read_as_of_the_time_the_facts_give
// test: the_quiet_time_of_a_changed_file_ends_that_long_after_its_time
// test: a_name_onedrive_cannot_hold_is_refused_with_its_reason
// test: the_plan_made_through_this_caller_converges_as_the_tests_caller_does

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};

use konedrive_tree::{
    is_same_content_but_for_time, make_conflict_copy_name, make_sync_plan, make_temporary_name, ContentVersion, FoldedName, FolderVisibility, Item, ItemCounts, ItemKind, ItemMemory, ItemPlace, ItemReader, ItemVersions, LocalItemId, NextTry, OneDriveItemReference, Reason,
    SyncPlan, SyncPlanInput, TreeError, TriedStep, WaitReason,
};

use super::local_step_tries::LocalStepTries;

/// What a plan is given that does not change while the sync runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSettings {
    /// This computer's name, for a conflict copy's name.
    pub computer_name: String,
    /// The largest size of a file OneDrive takes, in bytes.
    pub largest_file_size: u64,
    /// How long after its last change a file's content is not sent, in seconds.
    pub quiet_time: i64,
    /// How many unsent items, and how many OneDrive changed, are read at first.
    pub items_read_at_once: usize,
}

/// What the daemon knows beside the rows when a plan is made, as values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonFacts {
    /// Every item a step runs for (`RunningSteps::list_items`): the plan's
    /// `items_with_running_step`. A request to OneDrive that is not settled yet is among
    /// them by that alone: the step of a request whose answer was lost runs until it read
    /// what became of it (`upload::onedrive_steps::lost_answers`, which names the owner:
    /// `RunningSteps`). Nothing is kept across a stop: the next start sends nothing
    /// before a listing of its own ended.
    pub items_with_running_step: BTreeSet<LocalItemId>,
    /// The directories of OneDrive the `MakeDirectory`, `SendNewFile` and `SendMove`
    /// requests that are not settled yet send into (`RunningSteps::list_destinations`:
    /// `RunningStep::destination` of each running step, one whose answer was lost among
    /// them): their rows are the plan's `destinations_of_running_steps`.
    pub destinations_of_running_steps: Vec<OneDriveItemReference>,
    pub is_onedrive_full: bool,
    /// Unix seconds: the plan's `now`.
    pub now: i64,
    /// The rows the check at start left (`StartCheckFindings::held_items`): the plan's
    /// `items_left_by_start_up_check`, those of them that are given.
    pub left_by_start_up_check: BTreeSet<LocalItemId>,
    /// The tries the task keeps of the steps towards the folder and of the rows that
    /// failed, as they are when the round begins: a copy. The plan's reading voids those
    /// whose rows changed or went, and tells the plan the rest.
    pub local_step_tries: LocalStepTries,
    pub settings: PlanSettings,
}

/// The rows a plan is given, by id, with those of them whose relatives are still to be
/// given, those the plan was not told of yet, and what the steps remember of each that
/// was asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GivenItems {
    rows: BTreeMap<LocalItemId, Item>,
    /// The rows given whose relatives were not given yet (`close_given_items`).
    unclosed: Vec<LocalItemId>,
    /// The rows given that the plan's input does not say yet (`describe_given_items`).
    undescribed: Vec<LocalItemId>,
    /// What the steps remember of a row, read once in this reading.
    memories: BTreeMap<LocalItemId, ItemMemory>,
    /// The directories whose items below were given whole, with how many: the plan's
    /// `onedrive_item_count_below` and `shown_item_count_in`.
    onedrive_item_count_below: BTreeMap<LocalItemId, u64>,
    shown_item_count_in: BTreeMap<LocalItemId, u64>,
}

/// How many names of a conflict copy are tried for a name before none is taken as free.
const CONFLICT_COPY_ATTEMPTS: u32 = 100;

/// How many confirmed removals are read from the step memory at a time.
const CONFIRMED_REMOVALS_READ_AT_ONCE: usize = 1000;

/// The places `item` is at: the local one, OneDrive's and the one agreed on.
fn list_places(item: &Item) -> impl Iterator<Item = &ItemPlace> {
    item.local.iter().filter_map(|local| local.place.as_ref()).chain(item.onedrive.iter().filter_map(|onedrive| onedrive.place.as_ref())).chain(item.synced_place.iter())
}

/// Whether `item` is a directory that was removed here: the folder does not show it,
/// OneDrive has it, and the two sides agreed on a place of it. The one directory all
/// OneDrive has below is read for (`ItemReader::list_below_in_onedrive`).
fn is_directory_removed_here(item: &Item) -> bool {
    item.kind == ItemKind::Directory && item.local.is_none() && item.onedrive.is_some() && item.synced_place.is_some()
}

/// Whether `item` is unsent, or its OneDrive version is not the one agreed on, as the
/// store's two indexes have it: the time of a content alone is no change of OneDrive's.
fn is_differing(item: &Item) -> bool {
    let onedrive_place = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref());
    let onedrive_content = item.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref());
    item.unsent_since.is_some() || onedrive_place != item.synced_place.as_ref() || !is_same_content_but_for_time(onedrive_content, item.synced_content.as_ref())
}

impl GivenItems {
    /// Gives `rows`, each once.
    fn give(&mut self, rows: impl IntoIterator<Item = Item>) {
        for row in rows {
            if let std::collections::btree_map::Entry::Vacant(entry) = self.rows.entry(row.local_item_id) {
                self.unclosed.push(row.local_item_id);
                self.undescribed.push(row.local_item_id);
                entry.insert(row);
            }
        }
    }

    /// What the steps remember of `item`, read from the store the first time it is asked.
    fn find_memory(&mut self, items: &ItemReader<'_>, item: LocalItemId) -> Result<&ItemMemory, TreeError> {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.memories.entry(item) {
            entry.insert(items.find_item_memory(item)?);
        }
        Ok(&self.memories[&item])
    }

    /// Gives all OneDrive has in `directory` and below it, with its count.
    fn give_onedrive_items_below(&mut self, items: &ItemReader<'_>, directory: LocalItemId) -> Result<(), TreeError> {
        let below = items.list_below_in_onedrive(directory)?;
        self.onedrive_item_count_below.insert(directory, below.len() as u64);
        self.give(below);
        Ok(())
    }

    /// Gives what the folder shows in `directory`, with its count.
    fn give_shown_items_in(&mut self, items: &ItemReader<'_>, directory: LocalItemId) -> Result<(), TreeError> {
        let shown = items.list_children(directory)?;
        self.shown_item_count_in.insert(directory, shown.len() as u64);
        self.give(shown);
        Ok(())
    }
}

/// One round's plan, with what it was made from and what the task needs beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRound {
    pub input: SyncPlanInput,
    pub plan: SyncPlan,
    /// What the steps remember of each item given, as its row still says it: for the
    /// settlement's count of tries of a step towards OneDrive.
    pub memories: BTreeMap<LocalItemId, ItemMemory>,
    /// The task's tries of `facts.local_step_tries` that the rows of this reading still
    /// say (`LocalStepTries::void_by_rows`): what the task keeps from now on, in place of
    /// what it gave. Nothing else changes them between the two: the loop that makes the
    /// plan is the loop that settles the steps.
    pub local_step_tries: LocalStepTries,
    /// Whether every item that differs was read: when not, more is there than the plan
    /// was given.
    pub is_all_read: bool,
    /// The folder's counts for the bus, of the same reading as the plan.
    pub item_counts: ItemCounts,
}

/// The round's plan: reads `items_read_at_once` of each kind of item that differs
/// ([`read_differing_items`]), gives each with its relatives ([`close_given_items`]),
/// voids the task's tries whose rows changed or went ([`void_local_step_tries`]),
/// describes the folder ([`describe_folder`]) and them ([`describe_given_items`]), makes the plan (`make_sync_plan`); then, for
/// what the plan answers it was not given, gives it and makes the plan again: for
/// `WaitReason::ItemNotGiven(item)` the row; for `ItemsInDirectoryNotGiven(directory)`
/// ([`give_items_in_directory`]) all OneDrive has below a directory removed here, or what
/// the folder shows in one it shows, with its count. While no step can run and more is
/// there, it reads twice as many: since the plan is told what is refused and what waits
/// for its next try, a window whose every item waits has no step, and is widened — the
/// oldest unsent items being blocked starves nothing.
///
/// Called by the plan's task as ONE read closure (`Store::read_items`): it must not await
/// and reads nothing but the store. Bounded: the doubling ends when everything that
/// differs was read.
pub fn plan_next_steps(items: &ItemReader<'_>, facts: &DaemonFacts) -> Result<PlannedRound, TreeError> {
    let local_step_tries = void_local_step_tries(items, &facts.local_step_tries)?;
    let folder_input = describe_folder(items, facts)?;
    let item_counts = items.count_items()?;
    let mut read_at_once = facts.settings.items_read_at_once.max(1);
    loop {
        let (rows, is_all_read) = read_differing_items(items, read_at_once)?;
        let mut given = GivenItems::default();
        given.give(rows);
        let mut input = folder_input.clone();
        // No more askings than rows and directories: each gives what was not given.
        let mut directories_given = BTreeSet::new();
        let plan = loop {
            close_given_items(items, &mut given, &facts.settings.computer_name)?;
            describe_given_items(items, facts, &local_step_tries, &mut given, &mut input)?;
            let plan = make_sync_plan(&input);
            let mut is_more_given = false;
            for wait in &plan.waits {
                match wait.reason {
                    WaitReason::ItemNotGiven(item) if !given.rows.contains_key(&item) => match items.find_item(item)? {
                        Some(row) => {
                            given.give([row]);
                            is_more_given = true;
                        }
                        // The wait has no end by this plan; a later one finds the rows as they are then.
                        None => tracing::warn!("the sync plan waits for item {item} for item {}, and it has no row", wait.item),
                    },
                    WaitReason::ItemsInDirectoryNotGiven(directory) if directories_given.insert(directory) => {
                        give_items_in_directory(items, &mut given, directory)?;
                        is_more_given = true;
                    }
                    _ => {}
                }
            }
            if !is_more_given {
                break plan;
            }
        };
        // TODO: an item beyond the window is planned only when the plan of the window has no step at all (the review's S9): with a window full of unsent items that cannot be sent, newer ones wait while any step (a download, a change of OneDrive's) is planned; check and fix if needed.
        if !plan.steps.is_empty() || is_all_read {
            return Ok(PlannedRound { input, plan, memories: given.memories, local_step_tries, is_all_read, item_counts });
        }
        read_at_once = read_at_once.saturating_mul(2);
    }
}

/// The first `read_at_once` of: what is unsent (`list_unsent`, the oldest first), what
/// OneDrive changed (`list_changed_in_onedrive`), the shown items that cannot be shown any
/// more (`list_shown_items_that_cannot_be_shown`, by its index), and the pinned files that
/// are only to be downloaded (`list_pinned_files_to_download`: with every plan, as a plan
/// that left them out would drop the downloads the last one planned and never plan those
/// of a file a listing just showed); and whether each was read whole.
pub fn read_differing_items(items: &ItemReader<'_>, read_at_once: usize) -> Result<(Vec<Item>, bool), TreeError> {
    let unsent = items.list_unsent(None, read_at_once)?;
    let changed = items.list_changed_in_onedrive(None, read_at_once)?;
    let cannot_be_shown = items.list_shown_items_that_cannot_be_shown(None, read_at_once)?;
    // TODO: this reader scans all below the pinned directories with every plan, and every finished download makes a plan: N pinned downloads ending one by one are N scans (the review's S-c). A flag "no pinned file is missing its content", kept by the task and cleared by a pin, a listing's page and a content taken off the disk, would bound it; check the cost on a large pinned tree and fix if needed.
    let pinned = items.list_pinned_files_to_download(None, read_at_once)?;
    let is_all_read = [unsent.len(), changed.len(), cannot_be_shown.len(), pinned.len()].iter().all(|read| *read < read_at_once);
    Ok((unsent.into_iter().chain(changed).chain(cannot_be_shown).chain(pinned).collect(), is_all_read))
}

/// Gives, with every row of `given` that was not closed yet, what `SyncPlanInput` says
/// is given with it, until nothing is added: the directories above it on every side; for
/// one that differs, the items that have its name, a conflict copy's name it could take
/// (from the first attempt until one is free on both sides) or its temporary name, in
/// each directory it is in on any side; for a directory removed here all OneDrive has in
/// it; for a directory the folder shows, what the folder shows and OneDrive has in it;
/// and for a shown directory that cannot be shown, what the folder shows in it — each of
/// the last three with its count. A directory given for another's sake gets no count
/// here: the plan asks for it when it needs it ([`give_items_in_directory`]). A directory
/// OneDrive has that the folder does not show yet is given with nothing below it.
pub fn close_given_items(items: &ItemReader<'_>, given: &mut GivenItems, computer_name: &str) -> Result<(), TreeError> {
    while let Some(item) = given.unclosed.pop() {
        let Some(row) = given.rows.get(&item).cloned() else { continue };
        for place in list_places(&row) {
            given.give(items.find_item(place.parent_local_item_id)?);
        }
        let is_shown_directory = row.kind == ItemKind::Directory && row.local.is_some();
        // A directory the folder shows that OneDrive has where the folder cannot show it
        // may agree on everything: what the folder shows in it.
        if is_shown_directory && row.onedrive.is_some() && items.find_folder_visibility(item)? == FolderVisibility::CannotBeShown {
            given.give_shown_items_in(items, item)?;
        }
        if !is_differing(&row) {
            continue;
        }
        let directories: BTreeSet<LocalItemId> = list_places(&row).map(|place| place.parent_local_item_id).collect();
        let own_names: BTreeSet<OsString> = list_places(&row).map(|place| place.name.clone()).collect();
        let kept_temporary_name = given.find_memory(items, item)?.temporary_name.as_ref().map(|temporary_name| temporary_name.name.clone());
        for directory in directories {
            let mut names: Vec<OsString> = own_names.iter().cloned().collect();
            names.push(make_temporary_name(item, 1));
            names.extend(kept_temporary_name.clone());
            for name in &own_names {
                for attempt in 1..=CONFLICT_COPY_ATTEMPTS {
                    let candidate = make_conflict_copy_name(name, computer_name, attempt);
                    let folded = FoldedName::of(&candidate);
                    let is_free = items.list_children_with_folded_name(directory, &folded)?.is_empty() && items.list_children_in_onedrive_with_folded_name(directory, &folded)?.is_empty();
                    names.push(candidate);
                    if is_free {
                        break;
                    }
                }
            }
            for name in names {
                let folded = FoldedName::of(&name);
                given.give(items.list_children_with_folded_name(directory, &folded)?);
                given.give(items.list_children_in_onedrive_with_folded_name(directory, &folded)?);
            }
        }
        if is_directory_removed_here(&row) {
            given.give_onedrive_items_below(items, item)?;
        } else if is_shown_directory {
            given.give_shown_items_in(items, item)?;
            given.give(items.list_children_in_onedrive(item)?);
        }
    }
    Ok(())
}

/// The plan answered `WaitReason::ItemsInDirectoryNotGiven(directory)`: gives
/// `list_below_in_onedrive(directory)` with its length in `onedrive_item_count_below`
/// when the directory was removed here, or `list_children(directory)` with its length in
/// `shown_item_count_in` when the folder shows it and OneDrive lacks it or cannot show
/// it.
pub fn give_items_in_directory(items: &ItemReader<'_>, given: &mut GivenItems, directory: LocalItemId) -> Result<(), TreeError> {
    let row = match given.rows.get(&directory) {
        Some(row) => Some(row.clone()),
        None => items.find_item(directory)?,
    };
    let Some(row) = row else { return Ok(()) };
    if row.local.is_none() {
        given.give_onedrive_items_below(items, directory)?;
    } else {
        given.give_shown_items_in(items, directory)?;
    }
    given.give([row]);
    Ok(())
}

/// The task's tries that the rows still say: reads the row of every item a kept try was
/// planned from (`LocalStepTries::list_items`, `ItemReader::find_item`, `ItemVersions::of`
/// — an item that was not given to the plan too: a try is void by its rows, not by what
/// the plan reads this round) and leaves out every try one of whose rows went or is
/// another piece of work (`LocalStepTries::void_by_rows`). As many reads by id as there
/// are rows of steps that failed.
pub fn void_local_step_tries(items: &ItemReader<'_>, local_step_tries: &LocalStepTries) -> Result<LocalStepTries, TreeError> {
    let mut rows = BTreeMap::new();
    for item in local_step_tries.list_items() {
        if let Some(row) = items.find_item(item)? {
            rows.insert(item, ItemVersions::of(&row));
        }
    }
    let mut said = local_step_tries.clone();
    said.void_by_rows(&rows);
    Ok(said)
}

/// What the plan is told of the folder as a whole, with no row yet: read once for a plan,
/// whatever is given later. What the store says — whether a listing is under way, the
/// count of shown items, the confirmed removals (`list_items_with_confirmed_removal`,
/// those of items the plan is not given among them), the count of unsent removals, the
/// removals sent a short time ago (`find_sent_removal_window(facts.now)`, its
/// `removal_count`), the folder's state of sending — and what `facts` say: running, the
/// destinations of the running steps (each reference turned into its row by
/// `find_bound_item`), OneDrive full, the time, the settings.
pub fn describe_folder(items: &ItemReader<'_>, facts: &DaemonFacts) -> Result<SyncPlanInput, TreeError> {
    let mut items_with_confirmed_removal = BTreeSet::new();
    loop {
        let read = items.list_items_with_confirmed_removal(items_with_confirmed_removal.last().copied(), CONFIRMED_REMOVALS_READ_AT_ONCE)?;
        let is_last = read.len() < CONFIRMED_REMOVALS_READ_AT_ONCE;
        items_with_confirmed_removal.extend(read);
        if is_last {
            break;
        }
    }
    let mut destinations_of_running_steps = BTreeSet::new();
    for destination in &facts.destinations_of_running_steps {
        destinations_of_running_steps.extend(items.find_bound_item(destination)?.map(|row| row.local_item_id));
    }
    Ok(SyncPlanInput {
        items: Vec::new(),
        folder_visibility: BTreeMap::new(),
        ignored_items: BTreeSet::new(),
        name_refusals: BTreeMap::new(),
        items_open_for_writing: BTreeSet::new(),
        quiet_until: BTreeMap::new(),
        items_with_running_step: facts.items_with_running_step.clone(),
        destinations_of_running_steps,
        pinned_items: BTreeSet::new(),
        is_listing_under_way: items.find_listing_under_way()?.is_some(),
        is_onedrive_full: facts.is_onedrive_full,
        largest_file_size: facts.settings.largest_file_size,
        sending_state: items.find_sending_state()?,
        computer_name: facts.settings.computer_name.clone(),
        shown_item_count: items.count_shown_items()?,
        unsent_removal_count: items.count_unsent_removals(&items_with_confirmed_removal)?,
        items_with_confirmed_removal,
        places_taken_in_onedrive: BTreeMap::new(),
        items_left_by_uploads: BTreeMap::new(),
        temporary_names: BTreeMap::new(),
        sent_removal_count: items.find_sent_removal_window(facts.now)?.removal_count,
        now: facts.now,
        next_tries: BTreeMap::new(),
        sending_refusals: BTreeMap::new(),
        items_left_by_start_up_check: BTreeSet::new(),
        onedrive_item_count_below: BTreeMap::new(),
        shown_item_count_in: BTreeMap::new(),
    })
}

/// Tells `input` the rows of `given` it does not say yet, each once however often the
/// plan is made again: what the store says of the row (whether the folder can show it,
/// whether it is left here, whether a pin holds for it); what the row itself says is open
/// for writing (`content_before_write` set); what the step memory says of it — the names
/// OneDrive answered are taken in it (`list_taken_names` of each directory given), and
/// its memory, kept in `given`, with the temporary name it keeps
/// (`SyncPlanInput::temporary_names`), and whether it is an item an upload left with other
/// content (`ItemReader::find_mismatched_item` of its binding:
/// `SyncPlanInput::items_left_by_uploads`); and what `facts` say of it: left by the check, its quiet
/// time ([`decide_quiet_until`]), a name OneDrive cannot hold ([`find_name_refusal`]).
/// Then, of everything given so far: the rows by their id, the counts of the directories
/// given whole, the next tries ([`describe_next_tries`], with `local_step_tries`: the
/// task's own, voided by this reading) and the refusals ([`describe_sending_refusals`]).
pub fn describe_given_items(items: &ItemReader<'_>, facts: &DaemonFacts, local_step_tries: &LocalStepTries, given: &mut GivenItems, input: &mut SyncPlanInput) -> Result<(), TreeError> {
    for item in std::mem::take(&mut given.undescribed) {
        let Some(row) = given.rows.get(&item).cloned() else { continue };
        if let Some(temporary_name) = given.find_memory(items, item)?.temporary_name.clone() {
            input.temporary_names.insert(item, temporary_name);
        }
        // An item of OneDrive an upload of this folder left with other content, which a listing brought.
        if let Some(upload) = row.onedrive_item.as_ref().map(|onedrive_item| items.find_mismatched_item(onedrive_item)).transpose()?.flatten() {
            input.items_left_by_uploads.insert(item, upload.file);
        }
        if row.onedrive.is_some() {
            input.folder_visibility.insert(item, items.find_folder_visibility(item)?);
        }
        if items.is_ignored(item)? {
            input.ignored_items.insert(item);
        }
        let local_place = row.local.as_ref().and_then(|local| local.place.as_ref());
        if let Some(reason) = local_place.and_then(|place| find_name_refusal(&place.name)) {
            input.name_refusals.insert(item, reason);
        }
        if row.content_before_write.is_some() {
            input.items_open_for_writing.insert(item);
        }
        let local_content = row.local.as_ref().and_then(|local| local.content.as_ref());
        if let Some(until) = local_content.filter(|_| row.unsent_since.is_some()).and_then(|content| decide_quiet_until(content, facts.settings.quiet_time, facts.now)) {
            input.quiet_until.insert(item, until);
        }
        if row.kind == ItemKind::File && items.is_pinned(item)? {
            input.pinned_items.insert(item);
        }
        if row.kind == ItemKind::Directory {
            input.places_taken_in_onedrive.extend(items.list_taken_names(item)?.into_iter().map(|taken| (taken.item, taken.taken_name.place)));
        }
        if facts.left_by_start_up_check.contains(&item) {
            input.items_left_by_start_up_check.insert(item);
        }
        input.items.push(row);
    }
    input.items.sort_by_key(|row| row.local_item_id);
    input.onedrive_item_count_below.clone_from(&given.onedrive_item_count_below);
    input.shown_item_count_in.clone_from(&given.shown_item_count_in);
    input.next_tries = describe_next_tries(&given.memories, local_step_tries);
    input.sending_refusals = describe_sending_refusals(&given.memories);
    Ok(())
}

/// The plan's `next_tries`, from the two that keep tries. For each item whose memory keeps
/// failed tries with a time: that time and the reason of the last failure, under
/// `TriedStep::TowardsOneDrive` — the store keeps the tries of what is sent, and answers
/// them only for the unsent change they are of. And `LocalStepTries::list_next_tries` of
/// `local_step_tries`: the task's own, each under the kind of the step towards the folder
/// or of the rows it is of. Every entry is given, due or not: the plan compares with its
/// `now`. Pure.
pub fn describe_next_tries(memories: &BTreeMap<LocalItemId, ItemMemory>, local_step_tries: &LocalStepTries) -> BTreeMap<(LocalItemId, TriedStep), NextTry> {
    let describe_sent = |(item, memory): (&LocalItemId, &ItemMemory)| {
        let failed_tries = memory.failed_tries.as_ref()?;
        Some(((*item, TriedStep::TowardsOneDrive), NextTry { not_before: failed_tries.next_try_at?, reason: failed_tries.last_failure.clone() }))
    };
    let mut next_tries: BTreeMap<(LocalItemId, TriedStep), NextTry> = memories.iter().filter_map(describe_sent).collect();
    next_tries.extend(local_step_tries.list_next_tries());
    next_tries
}

/// The plan's `sending_refusals`: for each item OneDrive refuses for good as its row is
/// now (`Refusal`), that reason; else, for a file too big for the space there is
/// (`SpaceRefusal`), `Reason::TooBig` with what it needs and what is free. Pure.
pub fn describe_sending_refusals(memories: &BTreeMap<LocalItemId, ItemMemory>) -> BTreeMap<LocalItemId, Reason> {
    let describe = |(item, memory): (&LocalItemId, &ItemMemory)| {
        let refused = memory.refusal.as_ref().map(|refusal| refusal.reason.clone());
        let too_big = || memory.space_refusal.as_ref().map(|space_refusal| Reason::TooBig(Some((space_refusal.content.size(), space_refusal.free_space))));
        Some((*item, refused.or_else(too_big)?))
    };
    memories.iter().filter_map(describe).collect()
}

/// When the quiet time of a file with this local content ends, Unix seconds; `None` when
/// it ended. Pure. Counted from the content's modification time, which the last close
/// wrote from the backing file, and never later than `now + quiet_time` for a file whose
/// time is in the future. (A tool that sets old times loses the quiet time, which costs
/// a step that ends `ContentChanged`.)
pub fn decide_quiet_until(content: &ContentVersion, quiet_time: i64, now: i64) -> Option<i64> {
    let (changed_at, _) = content.mtime();
    let until = changed_at.saturating_add(quiet_time).min(now.saturating_add(quiet_time));
    (until > now).then_some(until)
}

/// Why OneDrive cannot hold `name`, when it cannot: today's rule
/// (`local/names.rs:51`, `refused`), unchanged. Design §6, "Names".
pub fn find_name_refusal(name: &OsStr) -> Option<Reason> {
    crate::local::names::refused(name).map(|refused| refused.reason())
}
