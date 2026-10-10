//! The plan for one item: the rows are written here as values, each as an operation of the
//! store leaves it. `existence`, `place` and `content` hold a test per cell of the design's
//! three tables (`docs/tasks/item-versions-design.md`, §6) and per sub-case the map of
//! today's code splits a cell into (`docs/tasks/item-versions-plan-map.txt`, §1: E1–E24,
//! P1–P11, C1–C11). A test's comment names the map's row and the test of today's code that
//! pins the same behaviour, or says "new" or "differs from today".

use std::collections::{BTreeMap, BTreeSet};

use konedrive_reason::Reason;

use super::item_decision::READ_ONLY_FOLDER;
use super::*;
use crate::model::{ContentPresence, ContentVersion, DriveId, ItemKind, ItemPlace, ItemVersion, OneDriveItemId, OneDriveItemReference, Placement};

mod content;
mod existence;
mod place;
mod item_relations;
mod enumeration;
mod convergence;

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
        is_listing_under_way: false,
        is_onedrive_full: false,
        largest_file_size: 250 << 30,
        sending_state: SendingState::ReadWrite,
        computer_name: "pc".to_owned(),
        shown_item_count: 1000,
        items_with_confirmed_removal: BTreeSet::new(),
        unsent_removal_count: 0,
        places_taken_in_onedrive: BTreeMap::new(),
    }
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

/// New.
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

/// New.
#[test]
fn items_the_two_sides_agree_on_are_in_no_part_of_the_answer() {
    let given = input(vec![synced_directory(2, ROOT, "d"), synced_file(3, id(2), "a.txt"), on_disk(synced_file(4, ROOT, "b.txt"))]);
    assert_eq!(make_sync_plan(&given), SyncPlan::default());
}

/// The map's E12 (a partial file at the path, today `not-downloaded` for 30 s, no test):
/// differs from today. The rows have no "being filled": a download is a step that is
/// running, and whatever the rows say of the item, it waits for that step. Also the map's
/// "one step per item at a time" (`T/outbox/pick.rs:186-196`).
#[test]
fn an_item_with_a_step_running_waits_for_it() {
    let mut given = input(vec![removed_in_onedrive(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt")), made_file(3, ROOT, "new.txt")]);
    given.items_with_running_step = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::RunningStep));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::RunningStep));
}

/// Today: `a_fresh_file_held_open_waits_for_its_last_writer`
/// (`fuse/detections/entry_created/tests.rs`) and
/// `a_onedrive_file_opened_for_writing_is_its_new_content_and_waits`
/// (`fuse/detections/file_written/tests.rs`).
#[test]
fn a_file_a_program_holds_open_for_writing_is_not_planned() {
    let mut given = input(vec![made_file(2, ROOT, "new.txt"), with_content_here(synced_file(3, ROOT, "a.txt"), written_here())]);
    given.items_open_for_writing = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::OpenForWriting)));
}

/// New: the step of an item that is both moved and changed here is its move, and its
/// content is sent by the plan made after it. Today one outbox row does both, the move
/// first (`edits_and_renames_go_up_guarded`, `upload/tests/row_kinds.rs`).
#[test]
fn an_item_has_one_step_and_its_place_goes_before_its_content() {
    let given = input(vec![with_content_here(moved_here(synced_file(2, ROOT, "a.txt"), ROOT, "b.txt"), written_here())]);
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 2), parent: reference("root"), name: "b.txt".into() }));
}

/// The design's "what is not sent: an ignored item". Today nothing is recorded for an
/// ignored name (`fuse/upload_rules.rs`); here the plan says it leaves the item out.
#[test]
fn an_ignored_item_is_left_out_and_waits_for_nothing() {
    let mut given = input(vec![made_directory(2, ROOT, "node_modules"), made_file(3, id(2), "a.js")]);
    given.ignored_items = [id(2), id(3)].into();
    assert_eq!(answer(&given, 2), Answer::Exclusion(ExclusionReason::NameIgnored));
    assert_eq!(answer(&given, 3), Answer::Exclusion(ExclusionReason::NameIgnored));
}

/// Today: `a_name_onedrive_refuses_or_a_file_too_large_waits_for_the_user`
/// (`fuse/detections/entry_created/tests.rs`).
#[test]
fn a_name_onedrive_cannot_hold_or_a_file_too_large_waits_for_the_user() {
    let mut given = input(vec![made_file(2, ROOT, "a:b"), made_file(3, ROOT, "large"), made_directory(4, ROOT, "CON"), moved_here(synced_file(5, ROOT, "a.txt"), ROOT, " a.txt")]);
    given.name_refusals = [(id(2), Reason::NameCharacters), (id(4), Reason::NameReserved), (id(5), Reason::NameSpaces)].into();
    given.largest_file_size = 2;
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::User(Reason::NameCharacters)));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::User(Reason::TooLarge)));
    assert_eq!(answer(&given, 4), Answer::Wait(WaitReason::User(Reason::NameReserved)));
    assert_eq!(answer(&given, 5), Answer::Wait(WaitReason::User(Reason::NameSpaces)), "a move sends the name too");
}

/// Today: `a_fresh_file_is_created_a_moment_after_it_is_closed`
/// (`fuse/detections/entry_created/tests.rs`): the reason is `changed-while-sending`.
#[test]
fn content_changed_within_the_quiet_time_waits_until_it_ends() {
    let mut given = input(vec![made_file(2, ROOT, "new.txt"), with_content_here(synced_file(3, ROOT, "a.txt"), written_here()), made_directory(4, ROOT, "d")]);
    given.quiet_until = [(id(2), 1002), (id(3), 1001), (id(4), 1003)].into();
    assert_eq!(answer(&given, 2), Answer::Wait(WaitReason::Time { until: 1002, reason: Reason::Changed }));
    assert_eq!(answer(&given, 3), Answer::Wait(WaitReason::Time { until: 1001, reason: Reason::Changed }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::MakeDirectory { item: versions(&given, 4), parent: reference("root"), name: "d".into() }), "a directory has no content");
}

/// Today: `a_full_onedrive_sends_no_content_but_moves_and_deletes_go`
/// (`upload/tests/space_left.rs`).
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

/// Differs from today, as the design says (§13, 3): a read-only folder sends what was
/// unsent when it was switched, and what came to be unsent later waits for the folder to be
/// read-write again. Today a switched folder sends nothing
/// (`unsent_changes_wait_while_the_folder_is_read_only`, `tests/fuse-probe/file_writes.sh`),
/// and a conflict copy made in one is not recorded
/// (`a_removed_directory_kept_in_a_read_only_folder_is_not_made_again`,
/// `fuse/conflict_copies/tests.rs`). Steps towards the folder go on.
#[test]
fn a_read_only_folder_sends_what_was_unsent_when_it_was_switched_and_nothing_later() {
    // The items 2 to 4 were unsent at the switch (their stamps are their numbers); 7 to 9
    // came to be unsent after it.
    let mut given = input(vec![
        made_file(2, ROOT, "new.txt"),
        moved_here(synced_file(3, ROOT, "b.txt"), ROOT, "c.txt"),
        removed_here(synced_file(4, ROOT, "d.txt")),
        made_file(7, ROOT, "copy.txt"),
        removed_here(synced_directory(8, ROOT, "gone")),
        with_content_here(synced_file(9, ROOT, "e.txt"), written_here()),
        listed(10, ItemKind::File, ROOT, "theirs.txt"),
    ]);
    given.sending_state = SendingState::ReadOnly { last_unsent_since: 4 };
    let waits_for_read_write = Answer::Wait(WaitReason::User(Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned()))));
    assert_eq!(answer(&given, 2), onedrive_step(OneDriveStep::SendNewFile { item: versions(&given, 2), parent: reference("root"), name: "new.txt".into() }));
    assert_eq!(answer(&given, 3), onedrive_step(OneDriveStep::SendMove { item: versions(&given, 3), parent: reference("root"), name: "c.txt".into() }));
    assert_eq!(answer(&given, 4), onedrive_step(OneDriveStep::SendFileRemoval { item: versions(&given, 4) }));
    assert_eq!(answer(&given, 7), waits_for_read_write);
    assert_eq!(answer(&given, 8), waits_for_read_write);
    assert_eq!(answer(&given, 9), waits_for_read_write);
    assert_eq!(answer(&given, 10), folder_step(FolderStep::ShowItem { item: versions(&given, 10) }));
}

/// New: a row the schema refuses (something of OneDrive's, or something agreed on, with no
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
