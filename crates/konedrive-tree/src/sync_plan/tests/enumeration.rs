//! The enumeration of related items: every kind of row the plan answers for by itself
//! ([`Shape`]), of a file and of a directory, against every kind for another item that has
//! to do with it ([`Relation`]) — the same directory and the same name as OneDrive compares
//! names, one in the other, one at the place the other leaves, the two exchanged, and three
//! in a circle. For each set of rows the rules of the rows allow, the plan must answer
//! something that can proceed ([`find_what_cannot_proceed`]): a step for at least one of
//! the items that wait for each other, or a wait that ends; never two waits for each
//! other, never nothing for an item the two sides do not agree on, and never a step the
//! rows it was planned from refuse ([`find_step_the_rows_refuse`]).
//!
//! The cases the plan decides between items are each tested by name in `item_relations`;
//! this is the gate they were written behind.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::folded_name::FoldedName;

/// A kind of row: what the two sides did to one item, or what the plan was told of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Agreed,
    MadeHere,
    NewInOneDrive,
    MovedHere,
    MovedInOneDrive,
    MovedOnBothSides,
    RemovedHere,
    RemovedInOneDrive,
    RemovedOnBothSides,
    BoundNeverListed,
    ChangedHere,
    ChangedInOneDrive,
    ChangedOnBothSides,
    RemovedInOneDriveChangedHere,
    RemovedHereChangedInOneDrive,
    Ignored,
    InQuietTime,
    NameRefused,
    OpenForWriting,
    StepRunning,
}

const EVERY_SHAPE: [Shape; 20] = [
    Shape::Agreed,
    Shape::MadeHere,
    Shape::NewInOneDrive,
    Shape::MovedHere,
    Shape::MovedInOneDrive,
    Shape::MovedOnBothSides,
    Shape::RemovedHere,
    Shape::RemovedInOneDrive,
    Shape::RemovedOnBothSides,
    Shape::BoundNeverListed,
    Shape::ChangedHere,
    Shape::ChangedInOneDrive,
    Shape::ChangedOnBothSides,
    Shape::RemovedInOneDriveChangedHere,
    Shape::RemovedHereChangedInOneDrive,
    Shape::Ignored,
    Shape::InQuietTime,
    Shape::NameRefused,
    Shape::OpenForWriting,
    Shape::StepRunning,
];

impl Shape {
    /// Whether a directory can be of this shape: the rest are about a file's content.
    fn is_of_directory_too(self) -> bool {
        !matches!(
            self,
            Shape::ChangedHere | Shape::ChangedInOneDrive | Shape::ChangedOnBothSides | Shape::RemovedInOneDriveChangedHere | Shape::RemovedHereChangedInOneDrive | Shape::InQuietTime | Shape::OpenForWriting
        )
    }

    /// The row of the item `number` of this shape. `at` is the place the shape is about:
    /// where the item is, or where it was moved to. `from` is where an item that was moved
    /// was agreed on.
    fn row(self, number: i64, kind: ItemKind, from: &ItemPlace, at: &ItemPlace) -> Item {
        let agreed = (kind == ItemKind::File).then(|| hashed(&format!("h{number}")));
        let mine = (kind == ItemKind::File).then(written_here);
        let theirs = (kind == ItemKind::File).then(|| hashed(&format!("theirs{number}")));
        let version = |place: &ItemPlace, content: &Option<ContentVersion>| Some(ItemVersion { place: Some(place.clone()), content: content.clone() });
        let elsewhere = ItemPlace { parent_local_item_id: from.parent_local_item_id, name: format!("{}-theirs", from.name.to_string_lossy()).into() };
        // (local, in OneDrive, the place agreed on, the content agreed on)
        let (local, onedrive, synced_place, synced_content) = match self {
            Shape::Agreed => (version(at, &agreed), version(at, &agreed), Some(at), &agreed),
            Shape::MadeHere | Shape::Ignored | Shape::InQuietTime | Shape::NameRefused | Shape::StepRunning => (version(at, &mine), None, None, &None),
            Shape::NewInOneDrive => (None, version(at, &agreed), None, &None),
            Shape::MovedHere => (version(at, &agreed), version(from, &agreed), Some(from), &agreed),
            Shape::MovedInOneDrive => (version(from, &agreed), version(at, &agreed), Some(from), &agreed),
            Shape::MovedOnBothSides => (version(at, &agreed), version(&elsewhere, &agreed), Some(from), &agreed),
            Shape::RemovedHere => (None, version(at, &agreed), Some(at), &agreed),
            Shape::RemovedInOneDrive => (version(at, &agreed), None, Some(at), &agreed),
            Shape::RemovedOnBothSides => (None, None, Some(at), &agreed),
            Shape::BoundNeverListed => (version(at, &agreed), None, None, &None),
            Shape::ChangedHere | Shape::OpenForWriting => (version(at, &mine), version(at, &agreed), Some(at), &agreed),
            Shape::ChangedInOneDrive => (version(at, &agreed), version(at, &theirs), Some(at), &agreed),
            Shape::ChangedOnBothSides => (version(at, &Some(hashed_here(&format!("mine{number}")))), version(at, &theirs), Some(at), &agreed),
            Shape::RemovedInOneDriveChangedHere => (version(at, &mine), None, Some(at), &agreed),
            Shape::RemovedHereChangedInOneDrive => (None, version(at, &theirs), Some(at), &agreed),
        };
        let is_bound = !matches!(self, Shape::MadeHere | Shape::Ignored | Shape::InQuietTime | Shape::NameRefused | Shape::StepRunning);
        let mut item = blank(number, kind);
        item.onedrive_item = is_bound.then(|| reference(&format!("I{number}")));
        item.onedrive_etag = onedrive.is_some().then(|| format!("e{number}"));
        item.unsent_since = (local.as_ref().and_then(|local| local.place.as_ref()) != synced_place || local.as_ref().and_then(|local| local.content.as_ref()) != synced_content.as_ref())
            .then_some(number as u64);
        if kind == ItemKind::File && local.as_ref().is_some_and(|local| local.content == mine) {
            item.content_presence = ContentPresence::OnDisk;
        }
        item.is_name_ignored = self == Shape::Ignored;
        item.synced_place = synced_place.cloned();
        item.synced_content = synced_content.clone();
        item.local = local;
        item.onedrive = onedrive;
        item
    }

    /// What the plan is told of the item `item` of this shape beside its row.
    fn tell(self, given: &mut SyncPlanInput, item: LocalItemId) {
        match self {
            Shape::Ignored => drop(given.ignored_items.insert(item)),
            Shape::InQuietTime => drop(given.quiet_until.insert(item, 1000)),
            Shape::NameRefused => drop(given.name_refusals.insert(item, Reason::NameCharacters)),
            Shape::OpenForWriting => drop(given.items_open_for_writing.insert(item)),
            Shape::StepRunning => drop(given.items_with_running_step.insert(item)),
            _ => {}
        }
    }
}

/// One item of an enumerated set: its shape and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Member {
    shape: Shape,
    kind: ItemKind,
}

fn every_member() -> Vec<Member> {
    let mut members = Vec::new();
    for shape in EVERY_SHAPE {
        members.push(Member { shape, kind: ItemKind::File });
        if shape.is_of_directory_too() {
            members.push(Member { shape, kind: ItemKind::Directory });
        }
    }
    members
}

/// What two items, the items 2 and 3, have to do with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// Both at one name of one directory.
    SameName,
    /// At two names of one directory that are one name to OneDrive.
    SameNameByCase,
    /// The second is in the first, a directory.
    Inside,
    /// The second was in the first, a directory, and is moved out of it.
    MovedOut,
    /// The second is at the place the first was moved from.
    AtPlaceLeft,
    /// Each is at the place the other was moved from.
    Exchanged,
    /// Each, a directory, is moved into the other.
    IntoEachOther,
}

const EVERY_RELATION: [Relation; 7] = [Relation::SameName, Relation::SameNameByCase, Relation::Inside, Relation::MovedOut, Relation::AtPlaceLeft, Relation::Exchanged, Relation::IntoEachOther];

impl Relation {
    /// The places of the items 2 and 3: for each, where it was moved from and where it is.
    fn places(self) -> [(ItemPlace, ItemPlace); 2] {
        match self {
            Relation::SameName => [(place(ROOT, "a-was"), place(ROOT, "x")), (place(ROOT, "b-was"), place(ROOT, "x"))],
            Relation::SameNameByCase => [(place(ROOT, "a-was"), place(ROOT, "x")), (place(ROOT, "b-was"), place(ROOT, "X"))],
            Relation::Inside => [(place(ROOT, "d-was"), place(ROOT, "d")), (place(ROOT, "b-was"), place(id(2), "x"))],
            Relation::MovedOut => [(place(ROOT, "d-was"), place(ROOT, "d")), (place(id(2), "x"), place(ROOT, "b-now"))],
            Relation::AtPlaceLeft => [(place(ROOT, "x"), place(ROOT, "a-now")), (place(ROOT, "b-was"), place(ROOT, "x"))],
            Relation::Exchanged => [(place(ROOT, "x"), place(ROOT, "y")), (place(ROOT, "y"), place(ROOT, "x"))],
            Relation::IntoEachOther => [(place(ROOT, "a"), place(id(3), "a")), (place(ROOT, "b"), place(id(2), "b"))],
        }
    }
}

/// Whether `items` are rows the rules of the rows allow (the design's §3): on each side an
/// item's directory exists there and is a directory, and no item is above itself; the
/// folder shows one item per name in a directory; OneDrive has one item per name in a
/// directory, as OneDrive compares names.
fn keep_the_rules_of_the_rows(items: &[Item]) -> bool {
    let by_id: BTreeMap<LocalItemId, &Item> = items.iter().map(|item| (item.local_item_id, item)).collect();
    let sides: [fn(&Item) -> Option<&ItemVersion>; 2] = [|item| item.local.as_ref(), |item| item.onedrive.as_ref()];
    for side in sides {
        for item in items {
            let mut above = side(item).and_then(|version| version.place.as_ref()).map(|place| place.parent_local_item_id);
            let mut steps = 0;
            while let Some(directory) = above {
                let Some(row) = by_id.get(&directory) else { return false };
                if row.kind != ItemKind::Directory || side(row).is_none() || directory == item.local_item_id || steps > items.len() {
                    return false;
                }
                above = side(row).and_then(|version| version.place.as_ref()).map(|place| place.parent_local_item_id);
                steps += 1;
            }
        }
    }
    let local_places: Vec<(LocalItemId, &std::ffi::OsStr)> = items.iter().filter_map(|item| item.local.as_ref()?.place.as_ref()).map(|place| (place.parent_local_item_id, place.name.as_os_str())).collect();
    let onedrive_places: Vec<(LocalItemId, FoldedName)> = items.iter().filter_map(|item| item.onedrive.as_ref()?.place.as_ref()).map(|place| (place.parent_local_item_id, FoldedName::of(&place.name))).collect();
    local_places.iter().collect::<BTreeSet<_>>().len() == local_places.len() && onedrive_places.iter().collect::<BTreeSet<_>>().len() == onedrive_places.len()
}

/// What the plan is given for the items of `members`, numbered from 2, at `places`; `None`
/// when the rules of the rows do not allow them together. An item made in a directory that
/// is left here is left here with it.
fn enumerated_input(members: &[Member], places: &[(ItemPlace, ItemPlace)]) -> Option<SyncPlanInput> {
    let rows: Vec<Item> = members.iter().zip(places).enumerate().map(|(index, (member, (from, at)))| member.shape.row(index as i64 + 2, member.kind, from, at)).collect();
    let mut given = input(rows);
    if !keep_the_rules_of_the_rows(&given.items) {
        return None;
    }
    for (index, member) in members.iter().enumerate() {
        member.shape.tell(&mut given, id(index as i64 + 2));
    }
    for _ in members {
        let left_here_too: Vec<LocalItemId> = given
            .items
            .iter()
            .filter(|item| item.onedrive_item.is_none())
            .filter(|item| item.local.as_ref().and_then(|local| local.place.as_ref()).is_some_and(|place| given.ignored_items.contains(&place.parent_local_item_id)))
            .map(|item| item.local_item_id)
            .collect();
        given.ignored_items.extend(left_here_too);
    }
    Some(given)
}

/// Whether the two sides do not agree on `item`, read off its row alone.
fn is_to_be_planned(item: &Item) -> bool {
    let place = |version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.place.clone());
    let content = |version: &Option<ItemVersion>| version.as_ref().and_then(|version| version.content.clone());
    item.local_item_id != ROOT
        && (item.local.is_some() != item.onedrive.is_some() || place(&item.local) != place(&item.onedrive) || place(&item.local) != item.synced_place || content(&item.local) != content(&item.onedrive) || content(&item.local) != item.synced_content)
}

/// The first step of `plan` that the rows of `given` refuse, in words: all the steps of one
/// plan can run now, in any order.
///
/// - A step that sends an item to a name of a directory of OneDrive, while the rows say
///   OneDrive has another item under that name there, as OneDrive compares names, or
///   another step of the plan sends one there.
/// - A step that brings an item to a name of a directory of the folder, while the folder
///   shows another item under that name, or another step of the plan brings one there.
/// - A step that moves a directory into what is in it, on the side it moves it on.
/// - A conflict copy's name that is taken on either side, as OneDrive compares names.
/// - Two steps for one item; the removal of a directory that would remove an item moved out
///   of it here, or beside the removal of what OneDrive has in it; a directory that gives
///   up its binding while the folder shows in it what the two sides agreed on there, unless
///   that item waits to be sent into it again; a merge with an item that has a step.
fn find_step_the_rows_refuse(given: &SyncPlanInput, plan: &SyncPlan) -> Option<String> {
    let folded = |place: &ItemPlace| (place.parent_local_item_id, FoldedName::of(&place.name));
    let local_place = |item: &Item| item.local.as_ref().and_then(|local| local.place.clone());
    let onedrive_place = |item: &Item| item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone());
    let is_in = |directory: LocalItemId, mut above: Option<LocalItemId>, place_of: &dyn Fn(&Item) -> Option<ItemPlace>| {
        for _ in 0..=given.items.len() {
            match above {
                Some(here) if here == directory => return true,
                Some(here) => above = given.items.iter().find(|item| item.local_item_id == here).and_then(place_of).map(|place| place.parent_local_item_id),
                None => return false,
            }
        }
        true
    };
    let mut sent_to: BTreeSet<(LocalItemId, FoldedName)> = BTreeSet::new();
    let mut brought_to: BTreeSet<(LocalItemId, std::ffi::OsString)> = BTreeSet::new();
    for step in &plan.steps {
        let number = step.local_item_id();
        let row = given.items.iter().find(|item| item.local_item_id == number)?;
        let sent = match step {
            SyncStep::OneDrive(OneDriveStep::MakeDirectory { parent, name, .. } | OneDriveStep::SendNewFile { parent, name, .. } | OneDriveStep::SendMove { parent, name, .. }) => {
                let Some(directory) = given.items.iter().find(|item| item.onedrive_item.as_ref() == Some(parent) && item.onedrive.is_some()) else {
                    return Some(format!("the step of item {number} sends it into a directory OneDrive does not have"));
                };
                Some(ItemPlace { parent_local_item_id: directory.local_item_id, name: name.clone() })
            }
            _ => None,
        };
        let brought = match step {
            SyncStep::Folder(FolderStep::ShowItem { .. } | FolderStep::MoveItem { .. }) => onedrive_place(row),
            SyncStep::Folder(FolderStep::TakeMoveBack { .. }) => row.synced_place.clone(),
            SyncStep::Folder(FolderStep::RenameToConflictCopy { name, .. }) => local_place(row).map(|place| ItemPlace { name: name.clone(), ..place }),
            SyncStep::Folder(FolderStep::MoveToTemporaryName { place, .. }) => Some(place.clone()),
            _ => None,
        };
        if let Some(sent) = &sent {
            if given.items.iter().any(|item| item.local_item_id != number && onedrive_place(item).is_some_and(|place| folded(&place) == folded(sent))) {
                return Some(format!("the step of item {number} sends it to a name OneDrive has another item under"));
            }
            if !sent_to.insert(folded(sent)) {
                return Some(format!("the step of item {number} sends it to a name another step sends an item to"));
            }
            if is_in(number, Some(sent.parent_local_item_id), &onedrive_place) {
                return Some(format!("the step of item {number} sends it into what OneDrive has in it"));
            }
        }
        if let Some(brought) = &brought {
            if given.items.iter().any(|item| item.local_item_id != number && local_place(item).as_ref() == Some(brought)) {
                return Some(format!("the step of item {number} brings it to a name the folder shows another item under"));
            }
            if !brought_to.insert((brought.parent_local_item_id, brought.name.clone())) {
                return Some(format!("the step of item {number} brings it to a name another step brings an item to"));
            }
            if is_in(number, Some(brought.parent_local_item_id), &local_place) {
                return Some(format!("the step of item {number} brings it into what the folder shows in it"));
            }
        }
        if plan.steps.iter().filter(|other| other.local_item_id() == number).count() > 1 {
            return Some(format!("item {number} has more than one step"));
        }
        let has_step = |item: LocalItemId| plan.steps.iter().any(|other| other.local_item_id() == item);
        match step {
            SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { .. }) => {
                let moved_out = |item: &&Item| is_in(number, onedrive_place(item).map(|place| place.parent_local_item_id), &onedrive_place) && item.local.is_some() && local_place(item) != item.synced_place;
                if let Some(moved_out) = given.items.iter().find(moved_out) {
                    return Some(format!("the removal of the directory {number} removes item {}, which was moved out of it here", moved_out.local_item_id));
                }
            }
            SyncStep::Row(RowStep::GiveUpBinding { .. }) if row.kind == ItemKind::Directory => {
                let was_agreed_on_in_it = |item: &Item| item.synced_place.as_ref().is_some_and(|place| place.parent_local_item_id == number) || onedrive_place(item).is_some_and(|place| place.parent_local_item_id == number);
                let waits_for_it = |item: &Item| plan.waits.iter().any(|wait| wait.item == item.local_item_id && wait.reason == WaitReason::AnotherItem(number));
                if let Some(shown) = given.items.iter().find(|item| local_place(item).is_some_and(|place| place.parent_local_item_id == number) && was_agreed_on_in_it(item) && !waits_for_it(item)) {
                    return Some(format!("the directory {number} gives up its binding while the folder shows item {} in it, which the two sides agreed on there and which is not sent into it again", shown.local_item_id));
                }
            }
            SyncStep::Row(RowStep::MergeItems { onedrive, .. }) if has_step(onedrive.local_item_id) => {
                return Some(format!("item {number} is merged with an item that has a step of its own"));
            }
            _ => {}
        }
        if matches!(step, SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. })) {
            let has_removal = |directory: LocalItemId| plan.steps.iter().any(|other| other.local_item_id() == directory && matches!(other, SyncStep::OneDrive(OneDriveStep::SendDirectoryRemoval { .. })));
            let in_removed_directory = |directory: &&Item| has_removal(directory.local_item_id) && is_in(directory.local_item_id, onedrive_place(row).map(|place| place.parent_local_item_id), &onedrive_place);
            if given.items.iter().any(|directory| in_removed_directory(&directory)) {
                return Some(format!("the removal of item {number} is sent beside that of a directory OneDrive has it in"));
            }
        }
        if let SyncStep::Folder(FolderStep::RenameToConflictCopy { .. }) = step {
            let name = brought.as_ref().map(folded);
            if given.items.iter().any(|item| item.local_item_id != number && (local_place(item).map(|place| folded(&place)) == name || onedrive_place(item).map(|place| folded(&place)) == name)) {
                return Some(format!("the conflict copy's name of item {number} is taken"));
            }
        }
    }
    None
}

/// What of `plan`, made from `given`, cannot proceed, in words; `None` when all of it can.
///
/// - The plan says it is at fault.
/// - The rows refuse one of its steps ([`find_step_the_rows_refuse`]).
/// - An item the two sides do not agree on has no step, no wait and no rule that leaves it
///   out.
/// - An item waits for a listing, and none is under way.
/// - An item waits for another item, and following the items that wait for one another
///   ends at no step and at no wait for a time, for the user or for a step that is running:
///   it ends at an item that was not given, at one that has no answer, at one a rule leaves
///   out, or at an item that was passed already.
fn find_what_cannot_proceed(given: &SyncPlanInput, plan: &SyncPlan) -> Option<String> {
    if !plan.faults.is_empty() {
        return Some(format!("fault {:?}", plan.faults));
    }
    if let Some(refused) = find_step_the_rows_refuse(given, plan) {
        return Some(refused);
    }
    let stepped: BTreeSet<LocalItemId> = plan.steps.iter().map(SyncStep::local_item_id).collect();
    let excluded: BTreeSet<LocalItemId> = plan.exclusions.iter().map(|exclusion| exclusion.item).collect();
    let waits: BTreeMap<LocalItemId, &WaitReason> = plan.waits.iter().map(|wait| (wait.item, &wait.reason)).collect();
    for item in &given.items {
        let number = item.local_item_id;
        if is_to_be_planned(item) && !stepped.contains(&number) && !excluded.contains(&number) && !waits.contains_key(&number) {
            return Some(format!("item {number} has no answer"));
        }
    }
    for wait in &plan.waits {
        let mut passed = BTreeSet::from([wait.item]);
        let mut reason = &wait.reason;
        loop {
            let another = match reason {
                WaitReason::Time { .. } | WaitReason::User(_) | WaitReason::RunningStep => break,
                WaitReason::Listing if given.is_listing_under_way => break,
                WaitReason::Listing => return Some(format!("item {} waits for a listing, and none is under way", wait.item)),
                WaitReason::AnotherItem(another) => *another,
                #[allow(unreachable_patterns)]
                other => return Some(format!("item {} waits as no set of given rows makes it wait: {other:?}", wait.item)),
            };
            if stepped.contains(&another) {
                break;
            }
            if excluded.contains(&another) {
                return Some(format!("item {} waits for item {another}, which a rule leaves out", wait.item));
            }
            if !passed.insert(another) {
                return Some(format!("item {} waits in a circle through item {another}", wait.item));
            }
            match waits.get(&another) {
                Some(next) => reason = next,
                None if given.items.iter().any(|item| item.local_item_id == another) => return Some(format!("item {} waits for item {another}, which has no answer", wait.item)),
                None => return Some(format!("item {} waits for item {another}, which was not given", wait.item)),
            }
        }
    }
    None
}

/// Every set of rows of the enumeration that the rules of the rows allow, each with the
/// words it is reported under.
pub(crate) fn every_enumerated_input() -> Vec<(String, SyncPlanInput)> {
    let members = every_member();
    let mut inputs = Vec::new();
    for relation in EVERY_RELATION {
        for first in &members {
            for second in &members {
                if let Some(given) = enumerated_input(&[*first, *second], &relation.places()) {
                    inputs.push((format!("{relation:?}: {:?} {:?} / {:?} {:?}", first.shape, first.kind, second.shape, second.kind), given));
                }
            }
        }
    }
    // Three in a circle. Of names: each is moved to the name the next was moved from. Of
    // directories: each is moved into the next.
    let movers = [Shape::MovedHere, Shape::MovedInOneDrive];
    let names = [(place(ROOT, "x"), place(ROOT, "y")), (place(ROOT, "y"), place(ROOT, "z")), (place(ROOT, "z"), place(ROOT, "x"))];
    let directories = [(place(ROOT, "a"), place(id(3), "a")), (place(ROOT, "b"), place(id(4), "b")), (place(ROOT, "c"), place(id(2), "c"))];
    for first in movers {
        for second in movers {
            for third in movers {
                for kind in [ItemKind::File, ItemKind::Directory] {
                    let circle = [first, second, third].map(|shape| Member { shape, kind });
                    if let Some(given) = enumerated_input(&circle, &names) {
                        inputs.push((format!("CircleOfNames: {first:?} / {second:?} / {third:?} {kind:?}"), given));
                    }
                }
                let circle = [first, second, third].map(|shape| Member { shape, kind: ItemKind::Directory });
                if let Some(given) = enumerated_input(&circle, &directories) {
                    inputs.push((format!("CircleOfDirectories: {first:?} / {second:?} / {third:?}"), given));
                }
            }
        }
    }
    inputs
}

/// New: the gate of the decisions between items, written before them. Of its 4529 sets of
/// rows, 242 could not proceed then: a step sent to a name OneDrive had another item under
/// (two items of one name as OneDrive compares names, an exchange, a place another item was
/// leaving), a directory sent or brought into what was in it, items waiting for each other
/// in a circle, and an item waiting for one a rule leaves out.
#[test]
fn every_two_related_items_and_every_three_in_a_circle_have_a_plan_that_can_proceed() {
    let inputs = every_enumerated_input();
    assert!(inputs.len() > 3000, "the enumeration lost its cells: {}", inputs.len());
    let cannot_proceed: Vec<String> = inputs.iter().filter_map(|(words, given)| find_what_cannot_proceed(given, &make_sync_plan(given)).map(|what| format!("{words}: {what}"))).collect();
    assert!(cannot_proceed.is_empty(), "{} of {} cannot proceed:\n{}", cannot_proceed.len(), inputs.len(), cannot_proceed.join("\n"));
}

/// The enumeration makes what it says: a set of rows the rules refuse is left out, and
/// every relation has cells.
#[test]
fn the_enumeration_holds_every_relation_and_no_rows_against_the_rules() {
    let inputs = every_enumerated_input();
    for relation in EVERY_RELATION {
        assert!(inputs.iter().any(|(words, _)| words.starts_with(&format!("{relation:?}:"))), "{relation:?}");
    }
    assert!(inputs.iter().any(|(words, _)| words.starts_with("CircleOfNames")) && inputs.iter().any(|(words, _)| words.starts_with("CircleOfDirectories")));
    // Two items the folder shows at one name are no rows; two that differ by case are.
    let made = Member { shape: Shape::MadeHere, kind: ItemKind::File };
    assert!(enumerated_input(&[made, made], &Relation::SameName.places()).is_none());
    assert!(enumerated_input(&[made, made], &Relation::SameNameByCase.places()).is_some());
    // A directory moved here into one that was moved here into it is above itself.
    let moved_here = Member { shape: Shape::MovedHere, kind: ItemKind::Directory };
    let moved_in_onedrive = Member { shape: Shape::MovedInOneDrive, kind: ItemKind::Directory };
    assert!(enumerated_input(&[moved_here, moved_here], &Relation::IntoEachOther.places()).is_none());
    assert!(enumerated_input(&[moved_here, moved_in_onedrive], &Relation::IntoEachOther.places()).is_some());
}
