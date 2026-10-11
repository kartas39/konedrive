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
//! (`next_tries`, `sending_refusals`) and what the check at start left
//! (`items_left_by_start_up_check`), which the plan is TOLD and answers as its own waits.
//!
//! **Tested without the rest**, over a store in memory:
// test: the_unsent_and_the_changed_in_onedrive_are_read_bounded
// test: the_pinned_to_download_are_read_only_when_the_facts_ask
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
// test: an_item_refused_for_good_or_too_big_for_the_space_is_given_as_refused_with_the_reason
// test: the_items_the_check_left_are_given_as_left_by_it
// test: the_rows_of_the_directories_running_steps_send_into_are_given_as_destinations
// test: the_removals_sent_a_short_time_ago_are_read_as_of_the_time_the_facts_give
// test: the_quiet_time_of_a_changed_file_ends_that_long_after_its_time
// test: a_name_onedrive_cannot_hold_is_refused_with_its_reason
// test: the_plan_made_through_this_caller_converges_as_the_tests_caller_does
#![allow(dead_code, unused_variables)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;

use konedrive_tree::{ContentVersion, Item, ItemMemory, ItemReader, LocalItemId, NextTry, OneDriveItemReference, Reason, SyncPlan, SyncPlanInput, TreeError};

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
    /// Every item a step runs for, and every item a request to OneDrive that is not
    /// settled yet is about: also one whose answer was lost, and one on record from before
    /// a stop, until its item was read from OneDrive (the plan's
    /// `items_with_running_step`).
    pub items_with_running_step: BTreeSet<LocalItemId>,
    /// The directories of OneDrive the `MakeDirectory`, `SendNewFile` and `SendMove`
    /// requests that are not settled yet send into (`RunningStep::destination`, and the
    /// records of requests whose answer was lost or that a stop left): their rows are the
    /// plan's `destinations_of_running_steps`.
    pub destinations_of_running_steps: Vec<OneDriveItemReference>,
    pub is_onedrive_full: bool,
    /// Unix seconds: the plan's `now`.
    pub now: i64,
    /// The rows the check at start left (`StartCheckFindings::held_items`): the plan's
    /// `items_left_by_start_up_check`, those of them that are given.
    pub left_by_start_up_check: BTreeSet<LocalItemId>,
    /// Whether this plan reads the pinned files to download: the one reader that scans.
    /// Asked after a listing's page and the user's answer, which alone change what it
    /// answers.
    pub reads_pinned_files: bool,
    pub settings: PlanSettings,
}

/// The rows a plan is given, by id, and those of them whose relatives were given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GivenItems {
    rows: BTreeMap<LocalItemId, Item>,
    closed: BTreeSet<LocalItemId>,
    /// The directories whose items below were given whole, with how many: the plan's
    /// `onedrive_item_count_below` and `shown_item_count_in`.
    onedrive_item_count_below: BTreeMap<LocalItemId, u64>,
    shown_item_count_in: BTreeMap<LocalItemId, u64>,
}

/// One round's plan, with what it was made from and what the task needs beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRound {
    pub input: SyncPlanInput,
    pub plan: SyncPlan,
    /// What the steps remember of each item given, as its row still says it: for the
    /// settlement's count of tries.
    pub memories: BTreeMap<LocalItemId, ItemMemory>,
    /// Whether every item that differs was read: when not, more is there than the plan
    /// was given.
    pub is_all_read: bool,
}

/// The round's plan: reads `items_read_at_once` of each kind of item that differs
/// ([`read_differing_items`]), gives each with its relatives ([`close_given_items`]),
/// describes them ([`describe_given_items`]), makes the plan (`make_sync_plan`); then, for
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
    todo!()
}

/// The first `read_at_once` of: what is unsent (`list_unsent`, the oldest first), what
/// OneDrive changed (`list_changed_in_onedrive`), the shown items that cannot be shown any
/// more (`list_shown_items_that_cannot_be_shown`, by its index), and — when `facts` ask —
/// the pinned files that are only to be downloaded (`list_pinned_files_to_download`); and
/// whether each was read whole.
pub fn read_differing_items(items: &ItemReader<'_>, facts: &DaemonFacts, read_at_once: usize) -> Result<(Vec<Item>, bool), TreeError> {
    todo!()
}

/// Gives, with every row of `given` that was not closed yet, what `SyncPlanInput` says
/// is given with it, until nothing is added: the directories above it on every side; for
/// one that differs, the items that have its name, a conflict copy's name it could take
/// (from the first attempt until one is free on both sides) or its temporary name, in
/// each directory it is in on any side; for a directory removed here all OneDrive has in
/// it; for a directory the folder shows, what the folder shows and OneDrive has in it;
/// and for a shown directory that cannot be shown, what the folder shows in it — each of
/// the last three with its count, as the test's caller always gives both counts for
/// every directory it gives.
pub fn close_given_items(items: &ItemReader<'_>, given: &mut GivenItems, computer_name: &str) -> Result<(), TreeError> {
    todo!()
}

/// The plan answered `WaitReason::ItemsInDirectoryNotGiven(directory)`: gives
/// `list_below_in_onedrive(directory)` with its length in `onedrive_item_count_below`
/// when the directory was removed here, or `list_children(directory)` with its length in
/// `shown_item_count_in` when the folder shows it and OneDrive lacks it or cannot show
/// it.
pub fn give_items_in_directory(items: &ItemReader<'_>, given: &mut GivenItems, directory: LocalItemId) -> Result<(), TreeError> {
    todo!()
}

/// What the plan is given for `given`: with what the store says of each row (whether the
/// folder can show it, whether it is left here, whether a pin holds for it, whether a
/// listing is under way, the count of shown items, the count of unsent removals, the
/// removals sent a short time ago (`find_sent_removal_window(facts.now)`, its
/// `removal_count`), the folder's state of sending); what the rows
/// themselves say is open for writing (`content_before_write` set); what the step memory
/// says — the confirmed removals (`list_items_with_confirmed_removal`), the names
/// OneDrive answered are taken (`list_taken_names` of each directory given), the next
/// tries ([`describe_next_tries`]) and the refusals ([`describe_sending_refusals`]); and
/// what `facts` say: running, the destinations of the running steps (each reference
/// turned into its row by `find_bound_item`), OneDrive full, the time, the rows left by
/// the check, the quiet times ([`decide_quiet_until`]), the names OneDrive cannot hold
/// ([`find_name_refusal`]). Also answers the memory of each item given.
pub fn describe_given_items(items: &ItemReader<'_>, facts: &DaemonFacts, given: &GivenItems) -> Result<(SyncPlanInput, BTreeMap<LocalItemId, ItemMemory>), TreeError> {
    todo!()
}

/// The plan's `next_tries`: for each item whose memory keeps failed tries with a time,
/// that time and the reason of the last failure. Every entry is given, due or not: the
/// plan compares with its `now`. An entry is of what the row still says: the step memory
/// answers no other. Pure.
pub fn describe_next_tries(memories: &BTreeMap<LocalItemId, ItemMemory>) -> BTreeMap<LocalItemId, NextTry> {
    todo!()
}

/// The plan's `sending_refusals`: for each item OneDrive refuses for good as its row is
/// now (`Refusal`), that reason; else, for a file too big for the space there is
/// (`SpaceRefusal`), `Reason::TooBig` with what it needs and what is free. Pure.
pub fn describe_sending_refusals(memories: &BTreeMap<LocalItemId, ItemMemory>) -> BTreeMap<LocalItemId, Reason> {
    todo!()
}

/// When the quiet time of a file with this local content ends, Unix seconds; `None` when
/// it ended. Pure. Counted from the content's modification time, which the last close
/// wrote from the backing file, and never later than `now + quiet_time` for a file whose
/// time is in the future. (A tool that sets old times loses the quiet time, which costs
/// a step that ends `ContentChanged`.)
pub fn decide_quiet_until(content: &ContentVersion, quiet_time: i64, now: i64) -> Option<i64> {
    todo!()
}

/// Why OneDrive cannot hold `name`, when it cannot: today's rule
/// (`local/names.rs:51`, `refused`), unchanged. Design §6, "Names".
pub fn find_name_refusal(name: &OsStr) -> Option<Reason> {
    todo!()
}
