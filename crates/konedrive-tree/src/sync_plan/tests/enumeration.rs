//! The enumeration of related items: every kind of row the plan answers for by itself
//! ([`Shape`]), of a file and of a directory, against every kind for another item that has
//! to do with it ([`Relation`]) — the same directory and the same name as OneDrive compares
//! names, one in the other, one at the place the other leaves, the two exchanged, a
//! directory in a directory with a third item in it or moved out of it, and three in a
//! circle. What only the caller of a plan knows is no kind of row: it is said of an item
//! of any shape ([`ItemFact`]) or of the folder ([`FolderFact`]), one at a time for every
//! relation, and each fact of an item with each fact of the folder where one item is in
//! another, was moved out of it, or has its name. For each set of rows the rules of the
//! rows allow, the plan must answer
//! something that can proceed ([`find_what_cannot_proceed`]): a step for at least one of
//! the items that wait for each other, or a wait that ends; never two waits for each
//! other, never nothing for an item the two sides do not agree on, and never a step the
//! rows it was planned from refuse ([`find_step_the_rows_refuse`]) or that what the caller
//! said rules out ([`find_step_the_callers_facts_refuse`]).
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
    /// Both sides have it, at two places, and nothing is agreed on of it: bound by a guess.
    BoundByGuess,
    ChangedHere,
    ChangedInOneDrive,
    ChangedOnBothSides,
    RemovedInOneDriveChangedHere,
    RemovedHereChangedInOneDrive,
    Ignored,
}

const EVERY_SHAPE: [Shape; 17] = [
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
    Shape::BoundByGuess,
    Shape::ChangedHere,
    Shape::ChangedInOneDrive,
    Shape::ChangedOnBothSides,
    Shape::RemovedInOneDriveChangedHere,
    Shape::RemovedHereChangedInOneDrive,
    Shape::Ignored,
];

impl Shape {
    /// Whether a directory can be of this shape: the rest are about a file's content.
    fn is_of_directory_too(self) -> bool {
        !matches!(
            self,
            Shape::ChangedHere | Shape::ChangedInOneDrive | Shape::ChangedOnBothSides | Shape::RemovedInOneDriveChangedHere | Shape::RemovedHereChangedInOneDrive
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
            Shape::MadeHere | Shape::Ignored => (version(at, &mine), None, None, &None),
            Shape::NewInOneDrive => (None, version(at, &agreed), None, &None),
            Shape::MovedHere => (version(at, &agreed), version(from, &agreed), Some(from), &agreed),
            Shape::MovedInOneDrive => (version(from, &agreed), version(at, &agreed), Some(from), &agreed),
            Shape::MovedOnBothSides => (version(at, &agreed), version(&elsewhere, &agreed), Some(from), &agreed),
            Shape::RemovedHere => (None, version(at, &agreed), Some(at), &agreed),
            Shape::RemovedInOneDrive => (version(at, &agreed), None, Some(at), &agreed),
            Shape::RemovedOnBothSides => (None, None, Some(at), &agreed),
            Shape::BoundNeverListed => (version(at, &agreed), None, None, &None),
            Shape::BoundByGuess => (version(at, &agreed), version(&elsewhere, &theirs), None, &None),
            Shape::ChangedHere => (version(at, &mine), version(at, &agreed), Some(at), &agreed),
            Shape::ChangedInOneDrive => (version(at, &agreed), version(at, &theirs), Some(at), &agreed),
            Shape::ChangedOnBothSides => (version(at, &Some(hashed_here(&format!("mine{number}")))), version(at, &theirs), Some(at), &agreed),
            Shape::RemovedInOneDriveChangedHere => (version(at, &mine), None, Some(at), &agreed),
            Shape::RemovedHereChangedInOneDrive => (None, version(at, &theirs), Some(at), &agreed),
        };
        let is_bound = !matches!(self, Shape::MadeHere | Shape::Ignored);
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
        if self == Shape::Ignored {
            given.ignored_items.insert(item);
        }
    }
}

/// What only the caller of a plan knows of one item: said of an item of any shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemFact {
    StepRunning,
    OpenForWriting,
    InQuietTime,
    NameRefused,
    /// The folder cannot show the item as OneDrive has it, nor what OneDrive has in it.
    CannotBeShown,
    /// A try of a sending failed, and the next is later.
    TriedAgainLater,
    /// OneDrive refused what was sent of it.
    SendingRefused,
    LeftByStartUpCheck,
    /// A step that is running sends an item into it, a directory.
    SentIntoByRunningStep,
}

const EVERY_ITEM_FACT: [ItemFact; 9] = [
    ItemFact::StepRunning,
    ItemFact::OpenForWriting,
    ItemFact::InQuietTime,
    ItemFact::NameRefused,
    ItemFact::CannotBeShown,
    ItemFact::TriedAgainLater,
    ItemFact::SendingRefused,
    ItemFact::LeftByStartUpCheck,
    ItemFact::SentIntoByRunningStep,
];

impl ItemFact {
    /// Whether a caller can say this of `item`: a step runs for any item; a program holds
    /// open a file the folder shows; the quiet time is of a file whose content was changed
    /// here; a name here is refused; and OneDrive has what the folder cannot show.
    fn can_be_said_of(self, item: &Item) -> bool {
        let content_here = item.local.as_ref().and_then(|local| local.content.as_ref());
        match self {
            ItemFact::StepRunning | ItemFact::LeftByStartUpCheck => true,
            ItemFact::TriedAgainLater | ItemFact::SendingRefused => item.unsent_since.is_some(),
            ItemFact::OpenForWriting => item.kind == ItemKind::File && item.local.is_some(),
            ItemFact::InQuietTime => content_here.is_some() && content_here != item.synced_content.as_ref(),
            ItemFact::NameRefused => item.local.is_some(),
            ItemFact::CannotBeShown => item.onedrive.is_some(),
            ItemFact::SentIntoByRunningStep => item.kind == ItemKind::Directory && item.onedrive.is_some(),
        }
    }

    fn tell(self, given: &mut SyncPlanInput, item: LocalItemId) {
        match self {
            ItemFact::StepRunning => drop(given.items_with_running_step.insert(item)),
            ItemFact::LeftByStartUpCheck => drop(given.items_left_by_start_up_check.insert(item)),
            ItemFact::SentIntoByRunningStep => drop(given.destinations_of_running_steps.insert(item)),
            ItemFact::TriedAgainLater => drop(given.next_tries.insert((item, TriedStep::TowardsOneDrive), NextTry { not_before: given.now + 60, reason: Reason::Network })),
            ItemFact::SendingRefused => drop(given.sending_refusals.insert(item, Reason::NotAllowed(None))),
            ItemFact::OpenForWriting => drop(given.items_open_for_writing.insert(item)),
            ItemFact::InQuietTime => drop(given.quiet_until.insert(item, 1000)),
            ItemFact::NameRefused => drop(given.name_refusals.insert(item, Reason::NameCharacters)),
            ItemFact::CannotBeShown => {
                // With the item, all OneDrive has in it, as the store answers it.
                let mut cannot_be_shown = BTreeSet::from([item]);
                for _ in 0..given.items.len() {
                    let in_them = given.items.iter().filter(|row| row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).is_some_and(|place| cannot_be_shown.contains(&place.parent_local_item_id)));
                    let in_them: Vec<LocalItemId> = in_them.map(|row| row.local_item_id).collect();
                    cannot_be_shown.extend(in_them);
                }
                for item in cannot_be_shown {
                    given.folder_visibility.insert(item, FolderVisibility::CannotBeShown);
                }
            }
        }
    }
}

/// What only the caller of a plan knows of the folder as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FolderFact {
    ListingUnderWay,
    /// Switched to read-only when item 2 was the last to have come to be unsent.
    ReadOnly,
    OneDriveFull,
    /// The unsent removals are a mass removal, and the user confirmed none.
    MassRemovalUnconfirmed,
}

const EVERY_FOLDER_FACT: [FolderFact; 4] = [FolderFact::ListingUnderWay, FolderFact::ReadOnly, FolderFact::OneDriveFull, FolderFact::MassRemovalUnconfirmed];

/// The last stamp given before the folder of [`FolderFact::ReadOnly`] was switched.
const LAST_UNSENT_SINCE_AT_SWITCH: u64 = 2;

impl FolderFact {
    fn tell(self, given: &mut SyncPlanInput) {
        match self {
            FolderFact::ListingUnderWay => given.is_listing_under_way = true,
            FolderFact::ReadOnly => given.sending_state = SendingState::ReadOnly { last_unsent_since: LAST_UNSENT_SINCE_AT_SWITCH },
            FolderFact::OneDriveFull => given.is_onedrive_full = true,
            FolderFact::MassRemovalUnconfirmed => given.unsent_removal_count = 600,
        }
    }
}

/// What the caller says beside the rows of one enumerated set: of each item at most one
/// fact, and of the folder at most one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CallerFacts {
    of_items: Vec<Option<ItemFact>>,
    of_folder: Option<FolderFact>,
}

/// The facts a set of `item_count` items is enumerated with. Always: nothing said; each
/// fact of an item said of one item; each fact of the folder. `is_crossed`: also each fact
/// of an item with each fact of the folder.
fn every_caller_facts(item_count: usize, is_crossed: bool) -> Vec<CallerFacts> {
    let nothing_of_items = vec![None; item_count];
    let mut of_one_item = vec![nothing_of_items.clone()];
    for item in 0..item_count {
        for fact in EVERY_ITEM_FACT {
            let mut of_items = nothing_of_items.clone();
            of_items[item] = Some(fact);
            of_one_item.push(of_items);
        }
    }
    let mut all: Vec<CallerFacts> = of_one_item.iter().map(|of_items| CallerFacts { of_items: of_items.clone(), of_folder: None }).collect();
    for of_folder in EVERY_FOLDER_FACT {
        let of_items = if is_crossed { of_one_item.clone() } else { vec![nothing_of_items.clone()] };
        all.extend(of_items.into_iter().map(|of_items| CallerFacts { of_items, of_folder: Some(of_folder) }));
    }
    all
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

/// What the items of a set, numbered from 2, have to do with each other.
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
    /// The third is in the second, a directory, which is in the first, a directory.
    InsideNested,
    /// The third was in the second, a directory in the first, a directory, and is moved out
    /// of both.
    MovedOutOfNested,
    /// The second and the third are both in the first, a directory.
    InsideTogether,
}

const EVERY_RELATION: [Relation; 10] = [
    Relation::SameName,
    Relation::SameNameByCase,
    Relation::Inside,
    Relation::MovedOut,
    Relation::AtPlaceLeft,
    Relation::Exchanged,
    Relation::IntoEachOther,
    Relation::InsideNested,
    Relation::MovedOutOfNested,
    Relation::InsideTogether,
];

impl Relation {
    /// The places of the items, from item 2 on: for each, where it was moved from and where
    /// it is.
    fn places(self) -> Vec<(ItemPlace, ItemPlace)> {
        match self {
            Relation::SameName => vec![(place(ROOT, "a-was"), place(ROOT, "x")), (place(ROOT, "b-was"), place(ROOT, "x"))],
            Relation::SameNameByCase => vec![(place(ROOT, "a-was"), place(ROOT, "x")), (place(ROOT, "b-was"), place(ROOT, "X"))],
            Relation::Inside => vec![(place(ROOT, "d-was"), place(ROOT, "d")), (place(ROOT, "b-was"), place(id(2), "x"))],
            Relation::MovedOut => vec![(place(ROOT, "d-was"), place(ROOT, "d")), (place(id(2), "x"), place(ROOT, "b-now"))],
            Relation::AtPlaceLeft => vec![(place(ROOT, "x"), place(ROOT, "a-now")), (place(ROOT, "b-was"), place(ROOT, "x"))],
            Relation::Exchanged => vec![(place(ROOT, "x"), place(ROOT, "y")), (place(ROOT, "y"), place(ROOT, "x"))],
            Relation::IntoEachOther => vec![(place(ROOT, "a"), place(id(3), "a")), (place(ROOT, "b"), place(id(2), "b"))],
            Relation::InsideNested => vec![(place(ROOT, "d-was"), place(ROOT, "d")), (place(ROOT, "e-was"), place(id(2), "e")), (place(ROOT, "b-was"), place(id(3), "x"))],
            Relation::MovedOutOfNested => vec![(place(ROOT, "d-was"), place(ROOT, "d")), (place(ROOT, "e-was"), place(id(2), "e")), (place(id(3), "x"), place(ROOT, "b-now"))],
            Relation::InsideTogether => vec![(place(ROOT, "d-was"), place(ROOT, "d")), (place(ROOT, "b-was"), place(id(2), "x")), (place(ROOT, "c-was"), place(id(2), "y"))],
        }
    }

    /// Whether a pass between items reads the answer or a fact of the other item here: one
    /// item is in the other, was moved out of it, or has its name. Each fact of an item is
    /// then enumerated with each fact of the folder.
    fn is_crossed_with_every_fact(self) -> bool {
        matches!(self, Relation::SameName | Relation::Inside | Relation::MovedOut | Relation::InsideNested | Relation::MovedOutOfNested)
    }

    /// The sets of members this relation is enumerated for. Two items: every member with
    /// every member. A directory in a directory: each of the two agreed on or removed, on
    /// either side or both, and every member as the third. Two items in one directory: the
    /// directory agreed on, moved here or removed on a side, every member as the second,
    /// and as the third a file the user removed, one the folder never showed, or one the
    /// user made.
    fn member_sets(self) -> Vec<Vec<Member>> {
        let members = every_member();
        let directory = |shape| Member { shape, kind: ItemKind::Directory };
        let nesting = [Shape::Agreed, Shape::RemovedHere, Shape::RemovedInOneDrive, Shape::RemovedOnBothSides];
        let mut sets = Vec::new();
        match self {
            Relation::InsideNested | Relation::MovedOutOfNested => {
                for outer in nesting {
                    for inner in nesting {
                        sets.extend(members.iter().map(|third| vec![directory(outer), directory(inner), *third]));
                    }
                }
            }
            // What a directory's removal or its leaving is decided from is all that is in
            // it: beside any second item, one the user removed, one the folder never
            // showed, or one the user made.
            Relation::InsideTogether => {
                let file = |shape| Member { shape, kind: ItemKind::File };
                for first in [Shape::Agreed, Shape::MovedHere, Shape::RemovedHere, Shape::RemovedInOneDrive] {
                    for third in [Shape::RemovedHere, Shape::NewInOneDrive, Shape::MadeHere] {
                        sets.extend(members.iter().map(|second| vec![directory(first), *second, file(third)]));
                    }
                }
            }
            _ => {
                for first in &members {
                    sets.extend(members.iter().map(|second| vec![*first, *second]));
                }
            }
        }
        sets
    }
}

/// Whether `items` are rows the rules of the rows allow (the design's §3): on each side an
/// item's directory exists there and is a directory, and no item is above itself; the
/// folder shows one item per name in a directory; OneDrive has one item per name in a
/// directory, the names compared as they were given: of two names that fold to one and are
/// not the same bytes OneDrive is the judge, and a listing that has ended may name both.
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
    let onedrive_places: Vec<(LocalItemId, &std::ffi::OsStr)> = items.iter().filter_map(|item| item.onedrive.as_ref()?.place.as_ref()).map(|place| (place.parent_local_item_id, place.name.as_os_str())).collect();
    local_places.iter().collect::<BTreeSet<_>>().len() == local_places.len() && onedrive_places.iter().collect::<BTreeSet<_>>().len() == onedrive_places.len()
}

/// What the plan is given for the items of `members`, numbered from 2, at `places`, with
/// nothing said by the caller; `None` when the rules of the rows do not allow them
/// together. An item made in a directory that is left here is left here with it.
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

/// `given`, with what the caller says in `facts` of its items, numbered from 2, and of the
/// folder; `None` when one of the facts cannot be said of its item.
fn tell_callers_facts(mut given: SyncPlanInput, facts: &CallerFacts) -> Option<SyncPlanInput> {
    for (index, fact) in facts.of_items.iter().enumerate() {
        let item = id(index as i64 + 2);
        let Some(fact) = fact else { continue };
        if !given.items.iter().find(|row| row.local_item_id == item).is_some_and(|row| fact.can_be_said_of(row)) {
            return None;
        }
        fact.tell(&mut given, item);
    }
    if let Some(fact) = facts.of_folder {
        fact.tell(&mut given);
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
/// - A move, on either side, of an item both sides have at two places with nothing agreed
///   on of it: its binding was a guess, and nobody moved it. Not so of a directory in which
///   the folder shows what the two sides agreed is in it.
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
            // Unless OneDrive has the item itself under a name of that fold there: it keeps
            // the two apart already.
            let has_the_name_already = onedrive_place(row).is_some_and(|place| folded(&place) == folded(sent));
            if !has_the_name_already && given.items.iter().any(|item| item.local_item_id != number && onedrive_place(item).is_some_and(|place| folded(&place) == folded(sent))) {
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
        let is_bound_by_guess = row.synced_place.is_none() && row.synced_content.is_none() && local_place(row).is_some() && onedrive_place(row).is_some() && local_place(row).map(|place| folded(&place)) != onedrive_place(row).map(|place| folded(&place));
        // A directory in which the folder shows what was agreed to be in it is no guess.
        let holds_what_was_agreed_on = given.items.iter().any(|item| local_place(item).is_some_and(|place| place.parent_local_item_id == number) && (item.synced_place.as_ref().is_some_and(|place| place.parent_local_item_id == number) || onedrive_place(item).is_some_and(|place| place.parent_local_item_id == number)));
        if is_bound_by_guess && !holds_what_was_agreed_on && matches!(step, SyncStep::OneDrive(OneDriveStep::SendMove { .. }) | SyncStep::Folder(FolderStep::MoveItem { .. } | FolderStep::TakeMoveBack { .. } | FolderStep::MoveToTemporaryName { .. })) {
            return Some(format!("item {number}, of which nothing is agreed on, is moved on a side though nobody moved it"));
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

/// The first step of `plan` that what the caller said in `given` rules out, in words.
///
/// - A step of an item a step is running for, that a program holds open for writing or
///   that the check at start left; a merge with an item that is held back so. A step
///   towards OneDrive of an item whose next try is later or that OneDrive refused.
/// - A removal sent to OneDrive while anything it would take with it has a step running,
///   is shown by the folder, where it waits to be moved out or is unsent, or was never shown
///   by the folder: every row the rows say OneDrive has below the removed directory, at any
///   depth. And the removal of an item that OneDrive has in a directory that was removed
///   here and whose removal is still to be sent: one removal, of the outermost.
/// - The move of a directory the folder cannot show.
/// - The removal of a directory while a listing is under way; a removal while the unsent
///   removals are a mass removal the user did not confirm.
/// - In a read-only folder, a step towards OneDrive of what came to be unsent after the
///   switch; while OneDrive is full, or within a file's quiet time, a sending of content;
///   a sending of a name OneDrive cannot hold.
/// - What the user made sent into a directory the folder cannot show as OneDrive has it,
///   or merged with an item it cannot show.
fn find_step_the_callers_facts_refuse(given: &SyncPlanInput, plan: &SyncPlan) -> Option<String> {
    let row_of = |item: LocalItemId| given.items.iter().find(|row| row.local_item_id == item);
    let directory_in_onedrive = |row: &Item| row.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.parent_local_item_id);
    let is_removed_here = |row: &Item| row.local.is_none() && row.onedrive.is_some() && row.synced_place.is_some();
    let cannot_be_shown = |item: LocalItemId| given.folder_visibility.get(&item) == Some(&FolderVisibility::CannotBeShown);
    for step in &plan.steps {
        let number = step.local_item_id();
        let row = row_of(number)?;
        if given.items_with_running_step.contains(&number) {
            return Some(format!("item {number} has a step while a step is running for it"));
        }
        if given.items_open_for_writing.contains(&number) {
            return Some(format!("item {number} has a step while a program holds it open for writing"));
        }
        if given.items_left_by_start_up_check.contains(&number) {
            return Some(format!("item {number} has a step though the check at start left it"));
        }
        if given.next_tries.contains_key(&(number, TriedStep::of(step))) {
            return Some(format!("item {number} has a step whose own next try is later"));
        }
        if matches!(step, SyncStep::OneDrive(_)) && given.sending_refusals.contains_key(&number) {
            return Some(format!("item {number} is sent though OneDrive refused it"));
        }
        let SyncStep::OneDrive(sending) = step else {
            if let SyncStep::Row(RowStep::MergeItems { onedrive, .. }) = step {
                if cannot_be_shown(onedrive.local_item_id) {
                    return Some(format!("item {number} is merged with item {}, which the folder cannot show", onedrive.local_item_id));
                }
                if given.items_with_running_step.contains(&onedrive.local_item_id) || given.items_left_by_start_up_check.contains(&onedrive.local_item_id) {
                    return Some(format!("item {number} is merged with item {}, which a step is running for", onedrive.local_item_id));
                }
            }
            continue;
        };
        let is_removal = matches!(sending, OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. });
        let sends_content = matches!(sending, OneDriveStep::SendNewFile { .. } | OneDriveStep::SendContent { .. });
        if is_removal {
            // A directory whose removal is of the folder alone is not to be removed.
            let forgets = |directory: LocalItemId| plan.steps.iter().any(|step| matches!(step, SyncStep::Row(RowStep::ForgetSyncedVersion { item }) if item.local_item_id == directory));
            let mut above = directory_in_onedrive(row);
            for _ in 0..given.items.len() {
                let Some(directory) = above.and_then(row_of) else { break };
                if is_removed_here(directory) && !forgets(directory.local_item_id) {
                    return Some(format!("the removal of item {number} is sent while the directory {} above it in OneDrive is still to be removed", directory.local_item_id));
                }
                above = directory_in_onedrive(directory);
            }
            if given.unsent_removal_count > 500 && !given.items_with_confirmed_removal.contains(&number) {
                return Some(format!("the removal of item {number} is sent in a mass removal the user did not confirm"));
            }
        }
        if matches!(sending, OneDriveStep::SendMove { .. }) && row.kind == ItemKind::Directory && cannot_be_shown(number) {
            return Some(format!("the move of the directory {number} is sent, which the folder cannot show"));
        }
        if let OneDriveStep::SendDirectoryRemoval { .. } = sending {
            if given.is_listing_under_way {
                return Some(format!("the removal of the directory {number} is sent while a listing is under way"));
            }
            if given.destinations_of_running_steps.contains(&number) {
                return Some(format!("the removal of the directory {number} is sent while a running step sends an item into it"));
            }
            let mut below = vec![number];
            for _ in 0..given.items.len() {
                let deeper: Vec<LocalItemId> = given.items.iter().filter(|row| directory_in_onedrive(row).is_some_and(|directory| below.contains(&directory)) && !below.contains(&row.local_item_id)).map(|row| row.local_item_id).collect();
                below.extend(deeper);
            }
            for taken in below.iter().skip(1).filter_map(|taken| row_of(*taken)) {
                if given.items_with_running_step.contains(&taken.local_item_id) || given.items_left_by_start_up_check.contains(&taken.local_item_id) || given.destinations_of_running_steps.contains(&taken.local_item_id) {
                    return Some(format!("the removal of the directory {number} takes with it item {}, which a step is running for or sends an item into, or the check at start left", taken.local_item_id));
                }
                if taken.local.is_some() {
                    return Some(format!("the removal of the directory {number} takes with it item {}, which the folder shows", taken.local_item_id));
                }
                if taken.synced_place.is_none() && taken.synced_content.is_none() {
                    return Some(format!("the removal of the directory {number} takes with it item {}, which the folder never showed", taken.local_item_id));
                }
            }
        }
        if let SendingState::ReadOnly { last_unsent_since } = given.sending_state {
            if row.unsent_since.is_none_or(|since| since > last_unsent_since) {
                return Some(format!("item {number} is sent in a read-only folder, and was not unsent when the folder was switched"));
            }
        }
        if sends_content && given.is_onedrive_full {
            return Some(format!("content of item {number} is sent while OneDrive is full"));
        }
        if sends_content && given.quiet_until.contains_key(&number) {
            return Some(format!("content of item {number} is sent within its quiet time"));
        }
        if let OneDriveStep::MakeDirectory { parent, name, .. } | OneDriveStep::SendNewFile { parent, name, .. } | OneDriveStep::SendMove { parent, name, .. } = sending {
            let name_here = row.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| &place.name);
            if given.name_refusals.contains_key(&number) && name_here == Some(name) {
                return Some(format!("item {number} is sent under a name OneDrive cannot hold"));
            }
            if let Some(directory) = given.items.iter().find(|row| row.onedrive_item.as_ref() == Some(parent)).filter(|directory| cannot_be_shown(directory.local_item_id)) {
                return Some(format!("item {number} is sent into the directory {}, which the folder cannot show", directory.local_item_id));
            }
        }
    }
    None
}

/// What of `plan`, made from `given`, cannot proceed, in words; `None` when all of it can.
///
/// - The plan says it is at fault.
/// - The rows refuse one of its steps ([`find_step_the_rows_refuse`]), or what the caller
///   said does ([`find_step_the_callers_facts_refuse`]).
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
    if let Some(refused) = find_step_the_rows_refuse(given, plan).or_else(|| find_step_the_callers_facts_refuse(given, plan)) {
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
                WaitReason::Time { .. } | WaitReason::User(_) | WaitReason::RunningStep | WaitReason::StartUpCheck => break,
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

/// Every set of rows of the enumeration that the rules of the rows allow, with every set
/// of facts the caller can say of them: `visit` is given each with the words it is reported
/// under.
pub(crate) fn visit_every_enumerated_input(mut visit: impl FnMut(&str, &SyncPlanInput)) {
    let mut visit_with_facts = |words: String, given: SyncPlanInput, item_count: usize, is_crossed: bool| {
        for facts in every_caller_facts(item_count, is_crossed) {
            if let Some(given) = tell_callers_facts(given.clone(), &facts) {
                let said: Vec<String> = facts.of_items.iter().enumerate().filter_map(|(index, fact)| Some(format!("{:?} of item {}", (*fact)?, index + 2))).chain(facts.of_folder.map(|fact| format!("{fact:?}"))).collect();
                visit(&if said.is_empty() { words.clone() } else { format!("{words} [{}]", said.join(", ")) }, &given);
            }
        }
    };
    for relation in EVERY_RELATION {
        for members in relation.member_sets() {
            if let Some(given) = enumerated_input(&members, &relation.places()) {
                let of_members: Vec<String> = members.iter().map(|member| format!("{:?} {:?}", member.shape, member.kind)).collect();
                visit_with_facts(format!("{relation:?}: {}", of_members.join(" / ")), given, members.len(), relation.is_crossed_with_every_fact());
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
                        visit_with_facts(format!("CircleOfNames: {first:?} / {second:?} / {third:?} {kind:?}"), given, 3, false);
                    }
                }
                let circle = [first, second, third].map(|shape| Member { shape, kind: ItemKind::Directory });
                if let Some(given) = enumerated_input(&circle, &directories) {
                    visit_with_facts(format!("CircleOfDirectories: {first:?} / {second:?} / {third:?}"), given, 3, false);
                }
            }
        }
    }
}

/// The gate of the decisions between items. Written before them, it held 4529 sets of rows,
/// of which 242 could not proceed then. Before the review's findings were mended it gained
/// what the caller says as facts of any item and of the folder, a directory in a directory,
/// and two items of OneDrive under names that fold to one.
#[test]
fn every_set_of_related_items_has_a_plan_that_can_proceed_whatever_the_caller_says() {
    let mut count = 0;
    let mut cannot_proceed: Vec<String> = Vec::new();
    visit_every_enumerated_input(|words, given| {
        count += 1;
        cannot_proceed.extend(find_what_cannot_proceed(given, &make_sync_plan(given)).map(|what| format!("{words}: {what}")));
    });
    assert!(count > 80_000, "the enumeration lost its cells: {count}");
    assert!(cannot_proceed.is_empty(), "{} of {count} cannot proceed:\n{}", cannot_proceed.len(), cannot_proceed.join("\n"));
}

/// The enumeration makes what it says: a set of rows the rules refuse is left out, and
/// every relation has cells.
#[test]
fn the_enumeration_holds_every_relation_and_no_rows_against_the_rules() {
    let mut all_words = Vec::new();
    visit_every_enumerated_input(|words, _| all_words.push(words.to_owned()));
    for relation in EVERY_RELATION {
        assert!(all_words.iter().any(|words| words.starts_with(&format!("{relation:?}:"))), "{relation:?}");
    }
    assert!(all_words.iter().any(|words| words.starts_with("CircleOfNames")) && all_words.iter().any(|words| words.starts_with("CircleOfDirectories")));
    for fact in EVERY_ITEM_FACT {
        assert!(all_words.iter().any(|words| words.contains(&format!("{fact:?} of item 3"))), "{fact:?}");
    }
    assert!(all_words.iter().any(|words| words.starts_with("MovedOutOfNested: RemovedHere Directory / RemovedHere Directory / MovedHere File [StepRunning of item 2]")));
    // Two items the folder shows at one name are no rows; two that differ by case are.
    let made = Member { shape: Shape::MadeHere, kind: ItemKind::File };
    assert!(enumerated_input(&[made, made], &Relation::SameName.places()).is_none());
    assert!(enumerated_input(&[made, made], &Relation::SameNameByCase.places()).is_some());
    // Two items OneDrive has at one name are no rows; under two names that fold to one,
    // they are: OneDrive is the judge.
    let new_in_onedrive = Member { shape: Shape::NewInOneDrive, kind: ItemKind::File };
    assert!(enumerated_input(&[new_in_onedrive, new_in_onedrive], &Relation::SameName.places()).is_none());
    assert!(enumerated_input(&[new_in_onedrive, new_in_onedrive], &Relation::SameNameByCase.places()).is_some());
    // A directory moved here into one that was moved here into it is above itself.
    let moved_here = Member { shape: Shape::MovedHere, kind: ItemKind::Directory };
    let moved_in_onedrive = Member { shape: Shape::MovedInOneDrive, kind: ItemKind::Directory };
    assert!(enumerated_input(&[moved_here, moved_here], &Relation::IntoEachOther.places()).is_none());
    assert!(enumerated_input(&[moved_here, moved_in_onedrive], &Relation::IntoEachOther.places()).is_some());
}
