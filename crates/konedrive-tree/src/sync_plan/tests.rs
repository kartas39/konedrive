//! The plan for one item: the rows are written here as values, each as an operation of the
//! store leaves it. `existence`, `place` and `content` hold a test per cell of the three
//! tables of `docs/design/sync.md` §6.3.

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use super::*;
use crate::model::{ContentPresence, ContentVersion, DriveId, ItemKind, ItemPlace, ItemVersion, OneDriveItemId, OneDriveItemReference, Placement};

mod content;
mod existence;
mod place;
mod item_relations;
mod directory_removal;
mod start_up_check;
mod temporary_names;
mod items_left_by_uploads;
mod own_upload_contents;
mod enumeration;
pub(crate) mod convergence;

pub(crate) const ROOT: LocalItemId = LocalItemId::ROOT;

pub(crate) fn id(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

pub(crate) fn reference(item_id: &str) -> OneDriveItemReference {
    OneDriveItemReference { drive_id: DriveId::new("d1").unwrap(), item_id: OneDriveItemId::new(item_id) }
}

pub(crate) fn place(parent: LocalItemId, name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: parent, name: name.into() }
}

/// A content OneDrive has, told by `hash`.
pub(crate) fn hashed(hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 100, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

/// A content a program wrote here, of the size of [`hashed`], with no hash known.
pub(crate) fn written_here() -> ContentVersion {
    ContentVersion::ChangedHere { size: 3, mtime: 200, mtime_nsec: 7, change_count: 1 }
}

/// The content [`written_here`] once it was hashed, as `hash`.
pub(crate) fn hashed_here(hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 200, mtime_nsec: 7, hash: Some(hash.to_owned()), ctag: None }
}

/// A row of `kind` with nothing on either side and nothing agreed on.
pub(crate) fn blank(number: i64, kind: ItemKind) -> Item {
    Item {
        local_item_id: id(number),
        onedrive_item: None,
        kind,
        local: None,
        onedrive: None,
        onedrive_etag: None,
        onedrive_mime: None,
        synced_place: None,
        synced_content: None,
        content_presence: ContentPresence::NotOnDisk,
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: None,
        is_executable: false,
    }
}

/// The root of the folder, bound to the root of the drive.
pub(crate) fn root() -> Item {
    let mut root = blank(1, ItemKind::Directory);
    root.onedrive_item = Some(reference("root"));
    root.local = Some(ItemVersion { place: None, content: None });
    root.onedrive = Some(ItemVersion { place: None, content: None });
    root
}

/// An item both sides have at `parent`/`name` and agree on, bound to the item `I<number>`
/// of OneDrive with the eTag `e<number>`; a file with the content `h<number>`, not on disk.
fn synced(number: i64, kind: ItemKind, parent: LocalItemId, name: &str) -> Item {
    let content = (kind == ItemKind::File).then(|| hashed(&format!("h{number}")));
    let version = ItemVersion { place: Some(place(parent, name)), content: content.clone() };
    let mut item = blank(number, kind);
    item.onedrive_item = Some(reference(&format!("I{number}")));
    item.local = Some(version.clone());
    item.onedrive = Some(version);
    item.onedrive_etag = Some(format!("e{number}"));
    item.synced_place = Some(place(parent, name));
    item.synced_content = content;
    item
}

pub(crate) fn synced_file(number: i64, parent: LocalItemId, name: &str) -> Item {
    synced(number, ItemKind::File, parent, name)
}

pub(crate) fn synced_directory(number: i64, parent: LocalItemId, name: &str) -> Item {
    synced(number, ItemKind::Directory, parent, name)
}

/// An item a program made here, which OneDrive does not have: a file with content on disk.
fn made(number: i64, kind: ItemKind, parent: LocalItemId, name: &str) -> Item {
    let mut item = blank(number, kind);
    item.local = Some(ItemVersion { place: Some(place(parent, name)), content: (kind == ItemKind::File).then(written_here) });
    item.content_presence = if kind == ItemKind::File { ContentPresence::OnDisk } else { ContentPresence::NotOnDisk };
    item.unsent_since = Some(number as u64);
    item
}

pub(crate) fn made_file(number: i64, parent: LocalItemId, name: &str) -> Item {
    made(number, ItemKind::File, parent, name)
}

pub(crate) fn made_directory(number: i64, parent: LocalItemId, name: &str) -> Item {
    made(number, ItemKind::Directory, parent, name)
}

/// An item a listing brought, which the folder does not show yet.
pub(crate) fn listed(number: i64, kind: ItemKind, parent: LocalItemId, name: &str) -> Item {
    let mut item = synced(number, kind, parent, name);
    item.local = None;
    item.synced_place = None;
    item.synced_content = None;
    item
}

/// `item`, removed in OneDrive, as a listing leaves its row.
pub(crate) fn removed_in_onedrive(mut item: Item) -> Item {
    item.onedrive = None;
    item.onedrive_etag = None;
    item
}

/// `item`, removed here, as the removal leaves a bound row.
pub(crate) fn removed_here(mut item: Item) -> Item {
    item.local = None;
    item.content_presence = ContentPresence::NotOnDisk;
    item.unsent_since = Some(item.local_item_id.as_i64() as u64);
    item
}

/// `item`, moved here to `parent`/`name`.
pub(crate) fn moved_here(mut item: Item, parent: LocalItemId, name: &str) -> Item {
    item.local.as_mut().unwrap().place = Some(place(parent, name));
    item.unsent_since = Some(item.local_item_id.as_i64() as u64);
    item
}

/// `item`, moved in OneDrive to `parent`/`name`, with another eTag.
pub(crate) fn moved_in_onedrive(mut item: Item, parent: LocalItemId, name: &str) -> Item {
    item.onedrive.as_mut().unwrap().place = Some(place(parent, name));
    item.onedrive_etag = Some("e-moved".to_owned());
    item
}

/// The file `item` with its whole content on disk.
pub(crate) fn on_disk(mut item: Item) -> Item {
    item.content_presence = ContentPresence::OnDisk;
    item
}

/// The file `item`, its content `content` here and on disk.
pub(crate) fn with_content_here(mut item: Item, content: ContentVersion) -> Item {
    item.local.as_mut().unwrap().content = Some(content);
    item.unsent_since = Some(item.local_item_id.as_i64() as u64);
    on_disk(item)
}

/// The file `item`, its content `content` in OneDrive, with another eTag.
pub(crate) fn with_content_in_onedrive(mut item: Item, content: ContentVersion) -> Item {
    item.onedrive.as_mut().unwrap().content = Some(content);
    item.onedrive_etag = Some("e-changed".to_owned());
    item
}

/// What a plan is given for `items` and the root, in a read-write folder with no listing
/// under way: the folder can show every item OneDrive has, and nothing else is said of any.
pub(crate) fn input(items: Vec<Item>) -> SyncPlanInput {
    let items: Vec<Item> = std::iter::once(root()).chain(items).collect();
    let (onedrive_item_count_below, shown_item_count_in) = count_items_in_directories(&items);
    let folder_visibility = items.iter().filter(|item| item.onedrive.is_some()).map(|item| (item.local_item_id, FolderVisibility::CanBeShown)).collect();
    SyncPlanInput {
        items,
        folder_visibility,
        ignored_items: BTreeSet::new(),
        name_refusals: BTreeMap::new(),
        items_open_for_writing: BTreeSet::new(),
        quiet_until: BTreeMap::new(),
        items_with_running_step: BTreeSet::new(),
        pinned_items: BTreeSet::new(),
        destinations_of_running_steps: BTreeSet::new(),
        is_listing_under_way: false,
        is_onedrive_full: false,
        computer_name: "pc".to_owned(),
        places_taken_in_onedrive: BTreeMap::new(),
        items_left_by_uploads: BTreeMap::new(),
        temporary_names: BTreeMap::new(),
        own_upload_contents: BTreeMap::new(),
        now: 500,
        next_tries: BTreeMap::new(),
        sending_refusals: BTreeMap::new(),
        items_left_by_start_up_check: BTreeSet::new(),
        onedrive_item_count_below,
        shown_item_count_in,
    }
}

/// For every directory of `items`: how many of `items` OneDrive has in it and below it, and
/// how many the folder shows in it. What a caller that gave all of them says.
pub(crate) fn count_items_in_directories(items: &[Item]) -> (BTreeMap<LocalItemId, u64>, BTreeMap<LocalItemId, u64>) {
    let rows = plan_items::PlanItems::of(items);
    let directories = || items.iter().filter(|item| item.kind == ItemKind::Directory).map(|item| item.local_item_id);
    (directories().map(|directory| (directory, rows.list_below_in_onedrive(directory).len() as u64)).collect(), directories().map(|directory| (directory, rows.list_shown_in(directory).len() as u64)).collect())
}

/// What a plan answers for one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Nothing,
    Step(SyncStep),
    Wait(WaitReason),
    Exclusion(ExclusionReason),
    /// The row is against the rules of the rows.
    Fault,
    /// Any other fault.
    OtherFault(PlanFault),
}

/// What the plan made from `input` answers for the item `number`: one thing at most.
pub(crate) fn answer(input: &SyncPlanInput, number: i64) -> Answer {
    let plan = make_sync_plan(input);
    let mut answers: Vec<Answer> = Vec::new();
    answers.extend(plan.steps.into_iter().filter(|step| step.local_item_id() == id(number)).map(Answer::Step));
    answers.extend(plan.waits.into_iter().filter(|wait| wait.item == id(number)).map(|wait| Answer::Wait(wait.reason)));
    answers.extend(plan.exclusions.into_iter().filter(|exclusion| exclusion.item == id(number)).map(|exclusion| Answer::Exclusion(exclusion.reason)));
    answers.extend(plan.faults.into_iter().filter(|fault| fault.local_item_id() == id(number)).map(|fault| match fault {
        PlanFault::RowAgainstRules(_) => Answer::Fault,
        other => Answer::OtherFault(other),
    }));
    assert!(answers.len() <= 1, "more than one answer for item {number}: {answers:?}");
    answers.pop().unwrap_or(Answer::Nothing)
}

/// The versions of the item `number` as `input` gives its row.
pub(crate) fn versions(input: &SyncPlanInput, number: i64) -> ItemVersions {
    ItemVersions::of(input.items.iter().find(|item| item.local_item_id == id(number)).expect("the item is given"))
}

pub(crate) fn onedrive_step(step: OneDriveStep) -> Answer {
    Answer::Step(SyncStep::OneDrive(step))
}

pub(crate) fn folder_step(step: FolderStep) -> Answer {
    Answer::Step(SyncStep::Folder(step))
}

pub(crate) fn row_step(step: RowStep) -> Answer {
    Answer::Step(SyncStep::Row(step))
}

#[test]
fn the_root_of_the_folder_is_never_planned() {
    // Also a root no listing bound yet, and one a program is said to write.
    let mut given = input(vec![]);
    assert_eq!(make_sync_plan(&given), SyncPlan::default());
    given.items[0].onedrive_item = None;
    given.items[0].onedrive = None;
    given.items_with_running_step.insert(ROOT);
    assert_eq!(make_sync_plan(&given), SyncPlan::default());
}

#[test]
fn items_the_two_sides_agree_on_are_in_no_part_of_the_answer() {
    let given = input(vec![synced_directory(2, ROOT, "d"), synced_file(3, id(2), "a.txt"), on_disk(synced_file(4, ROOT, "b.txt"))]);
    assert_eq!(make_sync_plan(&given), SyncPlan::default());
}

/// The rows have no "being filled": a download is a step that is running, and whatever
/// the rows say of the item, it waits for that step. One step per item at a time.
#[test]
fn an_item_with_a_step_running_waits_for_it() {
    let mut given = input(vec![removed_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt")), made_file(3, ROOT, "new.txt")]);
    given.items_with_running_step = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::RunningStep));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::RunningStep));
}

#[test]
fn a_file_a_program_holds_open_for_writing_is_not_planned() {
    let mut given = input(vec![made_file(2, ROOT, "new.txt"), with_content_here(synced_file(3, ROOT, "a.txt"), written_here())]);
    given.items_open_for_writing = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));
}

/// The step of an item that is both moved and changed here is its move, and its
/// content is sent by the plan made after it.
#[test]
fn an_item_has_one_step_and_its_place_goes_before_its_content() {
    let given = input(vec![with_content_here(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), written_here())]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "b.txt".into() }));
}

/// What is not sent: an ignored item (`docs/design/sync.md` §6.6). The plan says it leaves
/// the item out.
#[test]
fn an_ignored_item_is_left_out_and_waits_for_nothing() {
    let mut given = input(vec![made_directory(2, ROOT, "node_modules"), made_file(3, id(2), "a.js")]);
    given.ignored_items = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::NameIgnored));
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::NameIgnored));
}

#[test]
fn a_name_onedrive_cannot_hold_waits_for_the_user() {
    let mut given = input(vec![made_file(2, ROOT, "a:b"), made_directory(4, ROOT, "CON"), moved_here(synced_file(5, ROOT, "a.txt"), ROOT, " a.txt")]);
    given.name_refusals = [(id(2), Reason::NameCharacters), (id(4), Reason::NameReserved), (id(5), Reason::NameSpaces)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::NameCharacters)));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::User(Reason::NameReserved)));
    assert_eq!(answer(&given, 5), Answer::Wait(WaitReason::User(Reason::NameSpaces)), "a move sends the name too");
}

#[test]
fn content_changed_within_the_quiet_time_waits_until_it_ends() {
    let mut given = input(vec![made_file(2, ROOT, "new.txt"), with_content_here(synced_file(3, ROOT, "a.txt"), written_here()), made_directory(4, ROOT, "d")]);
    given.quiet_until = [(id(2), 1002), (id(3), 1001), (id(4), 1003)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Time { until: 1002, reason: Reason::QuietTime }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Time { until: 1001, reason: Reason::QuietTime }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 4), parent: reference("root"), name: "d".into() }), "a directory has no content");
}

#[test]
fn while_onedrive_is_full_no_content_is_sent_and_moves_and_removals_go_on() {
    let mut given = input(vec![
        made_file(2, ROOT, "new.txt"),
        with_content_here(synced_file(3, ROOT, "a.txt"), written_here()),
        moved_here(synced_file(4, ROOT, "b.txt"), ROOT, "c.txt"),
        removed_here(synced_file(5, ROOT, "d.txt")),
        made_directory(6, ROOT, "d"),
    ]);
    given.is_onedrive_full = true;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::WaitingForSpace)));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::WaitingForSpace)));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 4), parent: reference("root"), name: "c.txt".into() }));
    assert_eq!(answer(&given, 5), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 5) }));
    assert_eq!(answer(&given, 6), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 6), parent: reference("root"), name: "d".into() }));
}

/// A row the schema refuses (something of OneDrive's, or something agreed on, with no
/// binding) is a fault of the rows, said by the plan.
#[test]
fn a_row_the_rules_of_the_rows_refuse_is_a_fault() {
    let mut onedrive_version_with_no_binding = listed(2, ItemKind::File, ROOT, "a.txt");
    onedrive_version_with_no_binding.onedrive_item = None;
    let mut agreed_on_with_no_binding = made_file(3, ROOT, "b.txt");
    agreed_on_with_no_binding.synced_place = Some(place(ROOT, "b.txt"));
    let given = input(vec![onedrive_version_with_no_binding, agreed_on_with_no_binding]);
    assert_eq!(answer(&given, 2), Answer::Fault);
    assert_eq!(answer(&given, 3), Answer::Fault);
}
