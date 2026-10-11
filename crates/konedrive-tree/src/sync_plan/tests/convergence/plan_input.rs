//! The caller of the plan in miniature: what a plan is given, read from the store by its
//! bounded readers and closed as [`SyncPlanInput`] says, then widened by every item the plan
//! answers it was not given, until the plan asks for no more.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use crate::model::{Item, ItemKind, ItemPlace, LocalItemId};
use crate::sync_plan::name_exchange::make_temporary_name;
use crate::{make_conflict_copy_name, make_sync_plan, FoldedName, SendingState, SyncPlan, SyncPlanInput, TreeStore, WaitReason};

pub(crate) const COMPUTER_NAME: &str = "pc";

/// What only the caller of a plan knows, as these tests have it.
#[derive(Debug, Clone)]
pub(crate) struct CallerFacts {
    pub(crate) sending_state: SendingState,
    /// How many unsent items, and how many items OneDrive changed, are read at first.
    pub(crate) items_read_at_once: usize,
    /// Whether the directories above an item are given with it; when not, the plan has to
    /// ask for each it needs.
    pub(crate) gives_directories_above: bool,
    pub(crate) places_taken_in_onedrive: BTreeMap<LocalItemId, ItemPlace>,
    pub(crate) items_open_for_writing: BTreeSet<LocalItemId>,
    pub(crate) quiet_until: BTreeMap<LocalItemId, i64>,
    pub(crate) items_with_running_step: BTreeSet<LocalItemId>,
    pub(crate) is_onedrive_full: bool,
    pub(crate) destinations_of_running_steps: BTreeSet<LocalItemId>,
    /// The time, in Unix seconds: the plan's `now`.
    pub(crate) now: i64,
}

impl CallerFacts {
    pub(crate) fn of_read_write_folder() -> Self {
        Self {
            sending_state: SendingState::ReadWrite,
            items_read_at_once: 1000,
            gives_directories_above: true,
            places_taken_in_onedrive: BTreeMap::new(),
            items_open_for_writing: BTreeSet::new(),
            quiet_until: BTreeMap::new(),
            items_with_running_step: BTreeSet::new(),
            is_onedrive_full: false,
            destinations_of_running_steps: BTreeSet::new(),
            now: 0,
        }
    }
}

/// The rows a plan is given, by id, and those of them whose relatives were given already.
struct GivenItems {
    rows: BTreeMap<LocalItemId, Item>,
    closed: BTreeSet<LocalItemId>,
}

fn places_of(item: &Item) -> impl Iterator<Item = &ItemPlace> {
    item.local.iter().filter_map(|local| local.place.as_ref()).chain(item.onedrive.iter().filter_map(|onedrive| onedrive.place.as_ref())).chain(item.synced_place.iter())
}

/// Whether `item` is unsent or its OneDrive version is not the synced one, as the store's
/// two indexes have it: the time of a content is no change of OneDrive's.
fn differs(item: &Item) -> bool {
    let onedrive_content = item.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref());
    item.unsent_since.is_some() || item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()) != item.synced_place || !crate::sync_plan::side_change::is_same_content_but_for_time(onedrive_content, item.synced_content.as_ref())
}

impl GivenItems {
    fn give(&mut self, rows: impl IntoIterator<Item = Item>) {
        for row in rows {
            self.rows.entry(row.local_item_id).or_insert(row);
        }
    }

    /// Gives, with every item that differs on a side, what [`SyncPlanInput`] says is given
    /// with it, and with every item the directories above it; until nothing is added.
    fn close(&mut self, store: &mut TreeStore, gives_directories_above: bool) {
        loop {
            let Some(row) = self.rows.values().find(|row| !self.closed.contains(&row.local_item_id)).cloned() else { return };
            self.closed.insert(row.local_item_id);
            let items = store.item_store();
            if gives_directories_above {
                for place in places_of(&row) {
                    self.give(items.find_item(place.parent_local_item_id).unwrap());
                }
            }
            // A directory the folder shows that leaves it: what the folder shows in it. One
            // OneDrive removed differs; one OneDrive has where the folder cannot show it
            // may agree on everything, and the caller asks whether it can be shown.
            let cannot_be_shown = row.onedrive.is_some() && items.find_folder_visibility(row.local_item_id).unwrap() == crate::FolderVisibility::CannotBeShown;
            if row.kind == ItemKind::Directory && row.local.is_some() && cannot_be_shown {
                self.give(items.list_children(row.local_item_id).unwrap());
            }
            if !differs(&row) {
                continue;
            }
            // Those that have its name, a conflict copy's name it could take, or its
            // temporary name, in a directory it is in on either side.
            let directories: BTreeSet<LocalItemId> = places_of(&row).map(|place| place.parent_local_item_id).collect();
            let own_names: BTreeSet<std::ffi::OsString> = places_of(&row).map(|place| place.name.clone()).collect();
            for directory in directories {
                let mut names: Vec<std::ffi::OsString> = own_names.iter().cloned().collect();
                names.push(make_temporary_name(row.local_item_id, 1));
                for name in &own_names {
                    for attempt in 1..=100 {
                        let candidate = make_conflict_copy_name(name, COMPUTER_NAME, attempt);
                        let folded = FoldedName::of(&candidate);
                        let is_free = items.list_children_with_folded_name(directory, &folded).unwrap().is_empty() && items.list_children_in_onedrive_with_folded_name(directory, &folded).unwrap().is_empty();
                        names.push(candidate);
                        if is_free {
                            break;
                        }
                    }
                }
                for name in names {
                    let folded = FoldedName::of(&name);
                    self.give(items.list_children_with_folded_name(directory, &folded).unwrap());
                    self.give(items.list_children_in_onedrive_with_folded_name(directory, &folded).unwrap());
                }
            }
            if row.kind == ItemKind::Directory {
                // Removed here: all OneDrive has in it. Shown, and OneDrive does not have
                // it or it may leave the folder: what the folder shows in it.
                if row.local.is_none() {
                    self.give(items.list_below_in_onedrive(row.local_item_id).unwrap());
                } else {
                    self.give(items.list_children(row.local_item_id).unwrap());
                    self.give(items.list_children_in_onedrive(row.local_item_id).unwrap());
                }
            }
        }
    }
}

/// What a plan is given for `rows`, with what the store and `facts` say of each: of what
/// the steps remember, the temporary name each keeps, whether an upload left it, and
/// whether its content in OneDrive is the folder's own upload, as the daemon's caller reads
/// them.
fn describe(store: &mut TreeStore, facts: &CallerFacts, rows: Vec<Item>) -> SyncPlanInput {
    let items = store.item_store();
    let reader = items.reader();
    let temporary_names = rows.iter().filter_map(|row| Some((row.local_item_id, reader.find_item_memory(row.local_item_id).unwrap().temporary_name?))).collect();
    let items_left_by_uploads = rows.iter().filter_map(|row| Some((row.local_item_id, reader.find_mismatched_item(row.onedrive_item.as_ref()?).unwrap()?.file))).collect();
    let own_upload_contents = rows.iter().filter_map(|row| Some((row.local_item_id, reader.find_own_upload_content(row.local_item_id).unwrap()?))).collect();
    let name_refusal = |row: &Item| row.local.as_ref().and_then(|local| local.place.as_ref()).filter(|place| place.name.as_encoded_bytes().contains(&b':')).map(|_| Reason::NameCharacters);
    SyncPlanInput {
        folder_visibility: rows.iter().filter(|row| row.onedrive.is_some()).map(|row| (row.local_item_id, items.find_folder_visibility(row.local_item_id).unwrap())).collect(),
        ignored_items: rows.iter().filter(|row| items.is_ignored(row.local_item_id).unwrap()).map(|row| row.local_item_id).collect(),
        name_refusals: rows.iter().filter_map(|row| Some((row.local_item_id, name_refusal(row)?))).collect(),
        items_open_for_writing: facts.items_open_for_writing.clone(),
        quiet_until: facts.quiet_until.clone(),
        items_with_running_step: facts.items_with_running_step.clone(),
        destinations_of_running_steps: facts.destinations_of_running_steps.clone(),
        pinned_items: rows.iter().filter(|row| row.kind == ItemKind::File && items.is_pinned(row.local_item_id).unwrap()).map(|row| row.local_item_id).collect(),
        is_listing_under_way: items.find_listing_under_way().unwrap().is_some(),
        is_onedrive_full: facts.is_onedrive_full,
        largest_file_size: 250 << 30,
        sending_state: facts.sending_state,
        computer_name: COMPUTER_NAME.to_owned(),
        places_taken_in_onedrive: facts.places_taken_in_onedrive.clone(),
        items_left_by_uploads,
        temporary_names,
        own_upload_contents,
        now: facts.now,
        next_tries: BTreeMap::new(),
        sending_refusals: BTreeMap::new(),
        items_left_by_start_up_check: BTreeSet::new(),
        onedrive_item_count_below: rows.iter().filter(|row| row.kind == ItemKind::Directory).map(|row| (row.local_item_id, items.list_below_in_onedrive(row.local_item_id).unwrap().len() as u64)).collect(),
        shown_item_count_in: rows.iter().filter(|row| row.kind == ItemKind::Directory).map(|row| (row.local_item_id, items.list_children(row.local_item_id).unwrap().len() as u64)).collect(),
        items: rows,
    }
}

/// How the caller came to the plan it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WideningCount {
    /// How many items the plan answered it was not given, which were then given.
    pub(crate) items_asked_for: usize,
    /// How many times more items were read because no step could run.
    pub(crate) readings: usize,
}

/// The plan the caller runs next, and what it was made from. A bounded number of the unsent
/// items and of those OneDrive changed is read, the pinned files that are only to be
/// downloaded, and the shown items the folder cannot show any more; each is given with its relatives; and the plan is made again with every item
/// it answers it was not given. While no step can run and more is unsent or changed than was
/// read, twice as many are read.
pub(crate) fn plan_next_steps(store: &mut TreeStore, facts: &CallerFacts, widening: &mut WideningCount) -> (SyncPlanInput, SyncPlan) {
    let mut read_at_once = facts.items_read_at_once.max(1);
    loop {
        let mut given = GivenItems { rows: BTreeMap::new(), closed: BTreeSet::new() };
        let (unsent, changed, pinned) = {
            let items = store.item_store();
            (items.list_unsent(None, read_at_once).unwrap(), items.list_changed_in_onedrive(None, read_at_once).unwrap(), items.list_pinned_files_to_download(None, read_at_once).unwrap())
        };
        let cannot_be_shown = store.item_store().list_shown_items_that_cannot_be_shown(None, read_at_once).unwrap();
        let is_all_read = [unsent.len(), changed.len(), pinned.len(), cannot_be_shown.len()].iter().all(|read| *read < read_at_once);
        // The last of these: an item the folder shows and agrees on, whose placement a
        // listing changed, is in neither of the two indexes.
        given.give(unsent.into_iter().chain(changed).chain(pinned).chain(cannot_be_shown));
        // No more askings than rows: each adds one at least.
        let (input, plan) = loop {
            given.close(store, facts.gives_directories_above);
            let input = describe(store, facts, given.rows.values().cloned().collect());
            let plan = make_sync_plan(&input);
            let not_given: BTreeSet<LocalItemId> = plan.waits.iter().filter_map(|wait| if let WaitReason::ItemNotGiven(item) = wait.reason { Some(item) } else { None }).collect();
            if not_given.is_empty() {
                break (input, plan);
            }
            for item in not_given {
                assert!(!given.rows.contains_key(&item), "the plan answers that item {item} was not given, and it was");
                let row = store.item_store().find_item(item).unwrap().unwrap_or_else(|| panic!("the plan waits for item {item}, which has no row"));
                given.give([row]);
                widening.items_asked_for += 1;
            }
        };
        if !plan.steps.is_empty() || is_all_read {
            return (input, plan);
        }
        read_at_once *= 2;
        widening.readings += 1;
    }
}

/// What a plan is given when it is given every row.
pub(crate) fn describe_every_item(store: &mut TreeStore, facts: &CallerFacts) -> SyncPlanInput {
    let rows = store.item_store().list_items().unwrap();
    describe(store, facts, rows)
}
