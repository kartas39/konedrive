use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use konedrive_tree::{ContentPresence, ContentVersion, ExclusionReason, FactStanding, FailedTries, FailedTryStanding, ItemPlace, ItemVersion, Placement, PlanFault, Refusal, RefusedVersion, SkipReason, SpaceRefusal};

use super::*;
use crate::mount::item_fixture::{ItemFixture, START_TIME};
use crate::status::snapshot::{UploadQueueNote, SyncSnapshot};

const FOLDER: &str = "/folder";

fn id(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

fn content(size: u64) -> ContentVersion {
    ContentVersion::Hashed { size, mtime: 1, mtime_nsec: 0, hash: Some(format!("hash-{size}")), ctag: None }
}

fn place(name: &str) -> ItemPlace {
    ItemPlace { parent_local_item_id: LocalItemId::ROOT, name: name.into() }
}

/// A row made by hand for the pure decisions: an agreed file of 10 bytes at `a`, bound,
/// with a stamp; the cases change what they are about.
fn agreed_file() -> Item {
    Item {
        local_item_id: id(7),
        onedrive_item: Some(ItemFixture::onedrive_reference_of("a")),
        kind: ItemKind::File,
        local: Some(ItemVersion { place: Some(place("a")), content: Some(content(10)) }),
        onedrive: Some(ItemVersion { place: Some(place("a")), content: Some(content(10)) }),
        onedrive_etag: None,
        onedrive_mime: None,
        synced_place: Some(place("a")),
        synced_content: Some(content(10)),
        content_presence: ContentPresence::OnDisk,
        content_before_write: None,
        placement: Placement::Placed,
        is_name_ignored: false,
        confirmed_at_count: 0,
        unsent_since: Some(3),
        is_executable: false,
    }
}

/// A file whose content was changed here to 20 bytes: an `update`.
fn changed_file() -> Item {
    let mut item = agreed_file();
    item.local = Some(ItemVersion { place: Some(place("a")), content: Some(content(20)) });
    item
}

fn removed_file() -> Item {
    let mut item = agreed_file();
    item.local = None;
    item
}

fn entry(item: Item, answer: Option<ItemAnswer>) -> UnsentEntry {
    UnsentEntry { item, answer, running: None, memory: ItemMemory::default() }
}

fn waiting(reason: WaitReason) -> Option<ItemAnswer> {
    Some(ItemAnswer::Waiting(reason))
}

fn for_user(reason: Reason) -> Option<ItemAnswer> {
    waiting(WaitReason::User(reason))
}

fn running(hold: Option<SendingHold>) -> QueueConditions {
    QueueConditions { hold, is_onedrive_full: false, is_sync_running: true }
}

fn step(item: LocalItemId, class: StepClass, sent: u64, total: u64) -> RunningStep {
    RunningStep { class, item, other_items: Vec::new(), destination: None, sent, total }
}

fn refusal(reason: Reason) -> Refusal {
    Refusal { reason, version: RefusedVersion::Removal }
}

// ---- the kind ----

#[test]
fn an_unbound_directory_is_mkdir_an_unbound_file_create() {
    let mut file = agreed_file();
    (file.onedrive_item, file.onedrive, file.synced_place, file.synced_content) = (None, None, None, None);
    assert_eq!(decide_unsent_kind(&file), Some(UnsentKind::Create));
    let mut directory = file.clone();
    directory.kind = ItemKind::Directory;
    directory.local = Some(ItemVersion { place: Some(place("a")), content: None });
    assert_eq!(decide_unsent_kind(&directory), Some(UnsentKind::Mkdir));
    // Nothing agreed on yet, with a binding: still the item's first sending.
    file.onedrive_item = Some(ItemFixture::onedrive_reference_of("a"));
    assert_eq!(decide_unsent_kind(&file), Some(UnsentKind::Create));
}

#[test]
fn a_changed_content_is_update_also_when_the_item_was_moved_too() {
    assert_eq!(decide_unsent_kind(&changed_file()), Some(UnsentKind::Update));
    let mut moved_too = changed_file();
    moved_too.local = Some(ItemVersion { place: Some(place("b")), content: Some(content(20)) });
    assert_eq!(decide_unsent_kind(&moved_too), Some(UnsentKind::Update));
}

#[test]
fn a_place_that_differs_alone_is_move_and_no_local_version_is_delete() {
    let mut moved = agreed_file();
    moved.local = Some(ItemVersion { place: Some(place("b")), content: Some(content(10)) });
    assert_eq!(decide_unsent_kind(&moved), Some(UnsentKind::Move));
    assert_eq!(decide_unsent_kind(&removed_file()), Some(UnsentKind::Delete));
}

#[test]
fn an_item_with_no_stamp_or_that_agrees_has_no_kind() {
    assert_eq!(decide_unsent_kind(&agreed_file()), None, "a stamp, and nothing differs");
    let mut unstamped = changed_file();
    unstamped.unsent_since = None;
    assert_eq!(decide_unsent_kind(&unstamped), None);
}

#[test]
fn the_words_of_the_bus_are_todays() {
    let kinds = [UnsentKind::Mkdir, UnsentKind::Create, UnsentKind::Update, UnsentKind::Move, UnsentKind::Delete];
    assert_eq!(kinds.map(UnsentKind::as_str), ["mkdir", "create", "update", "move", "delete"]);
    assert_eq!(kinds.map(UnsentKind::sends_content), [false, true, true, false, false]);
    let states = [QueueState::Ready, QueueState::Running, QueueState::Retry, QueueState::Waiting, QueueState::Blocked, QueueState::Paused];
    assert_eq!(states.map(QueueState::as_str), ["ready", "running", "retry", "waiting", "blocked", "paused"]);
}

// ---- the state and the reason ----

/// The state and the reason of `entry` as a row of `Changes()` says them.
fn state_and_reason(entry: &UnsentEntry, sending: &QueueConditions) -> Option<(QueueState, Option<Reason>, i64)> {
    let kind = decide_unsent_kind(&entry.item)?;
    let state = decide_queue_state(entry, sending)?;
    let (reason, next_try) = decide_queue_reason(kind, state, entry, sending);
    Some((state, reason, next_try))
}

#[test]
fn every_answer_of_the_plan_has_its_word_and_its_reason() {
    let cases: Vec<(&str, Option<ItemAnswer>, QueueState, Option<Reason>, i64)> = vec![
        ("a planned step", Some(ItemAnswer::Planned(StepClass::Content)), QueueState::Ready, None, 0),
        ("a planned step of the rows", Some(ItemAnswer::Planned(StepClass::Row)), QueueState::Ready, None, 0),
        ("not given to the last plan", None, QueueState::Ready, None, 0),
        ("a name OneDrive cannot hold", for_user(Reason::NameCharacters), QueueState::Blocked, Some(Reason::NameCharacters), 0),
        ("larger than OneDrive takes", for_user(Reason::TooLarge), QueueState::Blocked, Some(Reason::TooLarge), 0),
        ("a file open for writing", for_user(Reason::OpenForWriting), QueueState::Waiting, Some(Reason::OpenForWriting), 0),
        ("a directory OneDrive does not have", for_user(Reason::Parent), QueueState::Waiting, Some(Reason::Parent), 0),
        ("a read-only folder's wait", for_user(Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned()))), QueueState::Waiting, Some(Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned()))), 0),
        ("OneDrive full", for_user(Reason::WaitingForSpace), QueueState::Ready, Some(Reason::WaitingForSpace), 0),
        ("too big for the space", for_user(Reason::TooBig(Some((20, 5)))), QueueState::Ready, Some(Reason::TooBig(Some((20, 5)))), 0),
        ("the quiet time", waiting(WaitReason::Time { until: 90, reason: Reason::QuietTime }), QueueState::Waiting, Some(Reason::QuietTime), 90),
        ("another item", waiting(WaitReason::AnotherItem(id(2))), QueueState::Waiting, None, 0),
        ("an item not given", waiting(WaitReason::ItemNotGiven(id(2))), QueueState::Waiting, None, 0),
        ("a running step that is not found", waiting(WaitReason::RunningStep), QueueState::Waiting, None, 0),
        ("a listing", waiting(WaitReason::Listing), QueueState::Waiting, None, 0),
        ("a directory not given whole", waiting(WaitReason::ItemsInDirectoryNotGiven(id(2))), QueueState::Waiting, None, 0),
        ("left by the check at start", waiting(WaitReason::StartUpCheck), QueueState::Waiting, Some(Reason::NotReadOnDisk), 0),
        ("a fault of the plan", Some(ItemAnswer::Fault(PlanFault::RowAgainstRules(id(7)))), QueueState::Waiting, Some(Reason::BeingChecked), 0),
    ];
    for (case, answer, state, reason, next_try) in cases {
        let entry = entry(changed_file(), answer);
        assert_eq!(state_and_reason(&entry, &running(None)), Some((state, reason, next_try)), "{case}");
    }
}

#[test]
fn a_wait_until_a_time_is_retry_when_a_failed_try_is_remembered_and_waiting_otherwise() {
    let answer = waiting(WaitReason::Time { until: 500, reason: Reason::Network });
    let mut retried = entry(changed_file(), answer.clone());
    retried.memory.failed_tries = Some(FailedTries { count: 2, next_try_at: Some(500), last_failure: Reason::Network });
    assert_eq!(state_and_reason(&retried, &running(None)), Some((QueueState::Retry, Some(Reason::Network), 500)));
    assert_eq!(state_and_reason(&entry(changed_file(), answer), &running(None)), Some((QueueState::Waiting, Some(Reason::Network), 500)));
}

#[test]
fn an_item_refused_for_good_is_blocked_whatever_its_reasons_group() {
    // OneDrive's `NotAllowed` is of the group `waiting`, and the plan answers it as it
    // answers a read-only folder's wait: the memory alone tells them apart.
    let not_allowed = Reason::NotAllowed(Some("no rights".to_owned()));
    for answer in [for_user(not_allowed.clone()), None] {
        let mut refused = entry(changed_file(), answer);
        refused.memory.refusal = Some(refusal(not_allowed.clone()));
        assert_eq!(state_and_reason(&refused, &running(None)), Some((QueueState::Blocked, Some(not_allowed.clone()), 0)));
        // Blocked also with no sync running, under a pause and in a read-only folder.
        for sending in [describe_stopped_sync(), running(Some(SendingHold::Paused { until: None })), running(Some(SendingHold::FolderReadOnly))] {
            assert_eq!(decide_queue_state(&refused, &sending), Some(QueueState::Blocked));
        }
    }
}

#[test]
fn an_item_the_last_plan_was_not_given_is_ready_or_what_the_memory_says() {
    let mut refused_for_space = entry(changed_file(), None);
    refused_for_space.memory.space_refusal = Some(SpaceRefusal { content: content(20), free_space: 5 });
    assert_eq!(state_and_reason(&refused_for_space, &running(None)), Some((QueueState::Ready, Some(Reason::TooBig(Some((20, 5)))), 0)));
    assert_eq!(state_and_reason(&entry(changed_file(), None), &running(None)), Some((QueueState::Ready, None, 0)));
}

#[test]
fn an_item_the_plan_leaves_out_is_no_entry() {
    for reason in [ExclusionReason::NameIgnored, ExclusionReason::CannotBeShown, ExclusionReason::LeftByUpload] {
        let left_out = entry(changed_file(), Some(ItemAnswer::Excluded(reason)));
        assert_eq!(decide_queue_state(&left_out, &running(None)), None);
        assert_eq!(describe_queue_change(&left_out, Path::new("a"), &running(None), Path::new(FOLDER)), None);
    }
}

#[test]
fn a_running_step_is_running_with_its_bytes() {
    let mut own = entry(changed_file(), waiting(WaitReason::RunningStep));
    own.running = Some(step(id(7), StepClass::Content, 4, 20));
    let row = describe_queue_change(&own, Path::new("a"), &running(None), Path::new(FOLDER)).expect("a row");
    assert_eq!((row.state.as_str(), row.sent, row.total, row.reason.as_str()), ("running", 4, 20, ""));

    // A step of another item that only carries this row says nothing of this entry's bytes.
    let mut carried = entry(removed_file(), waiting(WaitReason::RunningStep));
    carried.running = Some(RunningStep { other_items: vec![id(7)], ..step(id(2), StepClass::Metadata, 9, 9) });
    let row = describe_queue_change(&carried, Path::new("a"), &running(None), Path::new(FOLDER)).expect("a row");
    assert_eq!((row.state.as_str(), row.sent, row.total), ("running", 0, 0));
}

#[test]
fn a_row_says_the_stamp_the_full_path_and_how_far_a_kept_session_got() {
    let mut waiting_file = entry(changed_file(), Some(ItemAnswer::Planned(StepClass::Content)));
    let row = describe_queue_change(&waiting_file, Path::new("docs/a"), &running(None), Path::new(FOLDER)).expect("a row");
    assert_eq!(row, Change { seq: 3, kind: "update".into(), path: "/folder/docs/a".into(), state: "ready".into(), sent: 0, total: 20, reason: String::new(), next_try: 0 });
    waiting_file.memory.upload_session = Some(konedrive_tree::UploadSession {
        address: konedrive_tree::UploadSessionAddress::new("https://upload"),
        expires_at: None,
        next_offset: 12,
        content: content(20),
        empty_file_place: None,
    });
    let row = describe_queue_change(&waiting_file, Path::new("docs/a"), &running(None), Path::new(FOLDER)).expect("a row");
    assert_eq!((row.sent, row.total), (12, 20));
    // What sends no content has no bytes.
    let row = describe_queue_change(&entry(removed_file(), None), Path::new("a"), &running(None), Path::new(FOLDER)).expect("a row");
    assert_eq!((row.kind.as_str(), row.sent, row.total), ("delete", 0, 0));
    assert_eq!(describe_queue_change(&entry(agreed_file(), None), Path::new("a"), &running(None), Path::new(FOLDER)), None, "an item that agrees by now");
}

#[test]
fn every_entry_that_is_not_blocked_is_paused_while_the_account_is() {
    let paused = running(Some(SendingHold::Paused { until: Some(900) }));
    let mut in_flight = entry(changed_file(), waiting(WaitReason::RunningStep));
    in_flight.running = Some(step(id(7), StepClass::Content, 4, 20));
    let mut retried = entry(changed_file(), waiting(WaitReason::Time { until: 500, reason: Reason::Network }));
    retried.memory.failed_tries = Some(FailedTries { count: 1, next_try_at: Some(500), last_failure: Reason::Network });
    for paused_entry in [in_flight, retried, entry(changed_file(), None), entry(changed_file(), for_user(Reason::WaitingForSpace)), entry(changed_file(), for_user(Reason::OpenForWriting))] {
        assert_eq!(state_and_reason(&paused_entry, &paused), Some((QueueState::Paused, None, 0)));
    }
    assert_eq!(decide_queue_state(&entry(changed_file(), for_user(Reason::NameCharacters)), &paused), Some(QueueState::Blocked));
    // An item the check at start left, and a faulty one, keep their word too.
    assert_eq!(decide_queue_state(&entry(changed_file(), waiting(WaitReason::StartUpCheck)), &paused), Some(QueueState::Waiting));
    // No other hold changes a word.
    for hold in [SendingHold::Stopping, SendingHold::SignedOut, SendingHold::WritesNotAllowed { reason: "no".into() }, SendingHold::Throttled { until: 900 }, SendingHold::FirstListingNotEnded] {
        assert_eq!(decide_queue_state(&entry(changed_file(), None), &running(Some(hold))), Some(QueueState::Ready));
    }
}

#[test]
fn a_change_that_sends_content_waits_for_space_while_onedrive_is_full() {
    let full = QueueConditions { is_onedrive_full: true, ..running(None) };
    assert_eq!(state_and_reason(&entry(changed_file(), None), &full), Some((QueueState::Ready, Some(Reason::WaitingForSpace), 0)));
    assert_eq!(state_and_reason(&entry(changed_file(), waiting(WaitReason::Listing)), &full), Some((QueueState::Waiting, Some(Reason::WaitingForSpace), 0)));
    assert_eq!(state_and_reason(&entry(removed_file(), None), &full), Some((QueueState::Ready, None, 0)), "a removal sends no content");
    assert_eq!(state_and_reason(&entry(changed_file(), for_user(Reason::OpenForWriting)), &full), Some((QueueState::Waiting, Some(Reason::OpenForWriting), 0)), "it says something else");
}

#[test]
fn what_is_unsent_in_a_read_only_folder_or_with_no_sync_waits() {
    let read_only = running(Some(SendingHold::FolderReadOnly));
    let reason = Reason::NotAllowed(Some(READ_ONLY_FOLDER.to_owned()));
    assert_eq!(state_and_reason(&entry(changed_file(), Some(ItemAnswer::Planned(StepClass::Content))), &read_only), Some((QueueState::Waiting, Some(reason.clone()), 0)));
    assert_eq!(state_and_reason(&entry(changed_file(), None), &read_only), Some((QueueState::Waiting, Some(reason), 0)));
    let stopped = describe_stopped_sync();
    assert_eq!(stopped, QueueConditions { hold: None, is_onedrive_full: false, is_sync_running: false });
    assert_eq!(state_and_reason(&entry(changed_file(), None), &stopped), Some((QueueState::Waiting, None, 0)));
}

// ---- what is kept back ----

#[test]
fn what_is_kept_back_is_what_needs_the_user_or_waits_with_a_reason() {
    let sending = running(None);
    let kept = |answer: Option<ItemAnswer>| {
        let entry = entry(changed_file(), answer);
        let state = decide_queue_state(&entry, &sending).expect("an entry");
        decide_kept_back_reason(UnsentKind::Update, state, &entry, &sending)
    };
    assert_eq!(kept(for_user(Reason::NameCharacters)), Some(Reason::NameCharacters));
    assert_eq!(kept(for_user(Reason::OpenForWriting)), Some(Reason::OpenForWriting));
    assert_eq!(kept(waiting(WaitReason::Time { until: 5, reason: Reason::QuietTime })), Some(Reason::QuietTime));
    assert_eq!(kept(for_user(Reason::WaitingForSpace)), Some(Reason::WaitingForSpace));
    assert_eq!(kept(waiting(WaitReason::Listing)), None, "it waits with no reason");
    assert_eq!(kept(Some(ItemAnswer::Planned(StepClass::Content))), None, "it goes up by itself");
    let mut in_flight = entry(changed_file(), waiting(WaitReason::RunningStep));
    in_flight.running = Some(step(id(7), StepClass::Content, 1, 20));
    assert_eq!(decide_kept_back_reason(UnsentKind::Update, QueueState::Running, &in_flight, &sending), None);
    assert_eq!(decide_kept_back_reason(UnsentKind::Update, QueueState::Blocked, &entry(changed_file(), None), &sending), Some(Reason::Blocked), "blocked with no reason");
}

/// Six entries kept back for four reasons, and one that is not.
fn kept_back_entries() -> Vec<UnsentEntry> {
    let numbered = |number: i64, size: u64, answer: Option<ItemAnswer>| {
        let mut item = changed_file();
        item.local_item_id = id(number);
        item.local = Some(ItemVersion { place: Some(place("a")), content: Some(content(size)) });
        entry(item, answer)
    };
    let mut refused = numbered(15, 7, None);
    refused.memory.refusal = Some(refusal(Reason::NotAllowed(Some("no rights".to_owned()))));
    vec![
        numbered(14, 100, for_user(Reason::OpenForWriting)),
        refused,
        numbered(13, 30, for_user(Reason::WaitingForSpace)),
        numbered(12, 2, for_user(Reason::NameCharacters)),
        numbered(11, 3, for_user(Reason::NameCharacters)),
        numbered(10, 50, for_user(Reason::TooBig(Some((50, 5))))),
        numbered(16, 1, Some(ItemAnswer::Planned(StepClass::Content))),
    ]
}

fn summary_row(group: &str, reason: &Reason, count: u32, bytes: u64) -> KeptBackReason {
    KeptBackReason { group: group.to_owned(), reason: reason.key().to_owned(), count, bytes }
}

fn kept_back_summary() -> Vec<KeptBackReason> {
    let not_allowed = Reason::NotAllowed(None);
    vec![
        summary_row("one-action", &Reason::TooBig(Some((50, 5))), 1, 50),
        summary_row("one-action", &Reason::WaitingForSpace, 1, 30),
        summary_row("per-file", &Reason::NameCharacters, 2, 5),
        // Refused for good: per file, though the reason's own group is `waiting`.
        summary_row("per-file", &not_allowed, 1, 7),
        summary_row("waiting", &Reason::OpenForWriting, 1, 100),
    ]
}

#[test]
fn the_summary_groups_what_is_kept_back_by_reason_in_the_groups_order() {
    assert_eq!(summarize_kept_back(&kept_back_entries(), &running(None)), kept_back_summary());
}

#[test]
fn a_pause_changes_neither_what_is_kept_back_nor_the_counts() {
    let entries = kept_back_entries();
    let paused = running(Some(SendingHold::Paused { until: None }));
    assert_eq!(summarize_kept_back(&entries, &paused), kept_back_summary());
    assert_eq!(list_entries_kept_back_for(&entries, &paused, Reason::WaitingForSpace.key()).len(), 1);
    assert_eq!(list_blocked_entries(&entries, &paused).len(), 3);
    let totals = QueueTotals { unsent: UnsentTotal { count: 8, bytes: 194 }, refused_for_good: vec![id(15)], refused_for_space: vec![(id(10), 50)], answered: entries };
    assert_eq!(count_queue(&totals, &paused), count_queue(&totals, &running(None)));
    assert_eq!(count_queue(&totals, &paused).space_waiting, 1);
}

#[test]
fn what_is_not_uploaded_is_listed_per_file_with_its_reason() {
    let entries = kept_back_entries();
    let paths: BTreeMap<LocalItemId, PathBuf> = [(11, "z"), (12, "b"), (15, "c"), (13, "s")].into_iter().map(|(number, path)| (id(number), PathBuf::from(path))).collect();
    let blocked: Vec<LocalItemId> = list_blocked_entries(&entries, &running(None)).iter().map(|entry| entry.item.local_item_id).collect();
    assert_eq!(blocked, [id(15), id(12), id(11)]);
    let name = Reason::NameCharacters.to_string();
    let listed = list_not_uploaded(&entries, &paths, &running(None), Path::new(FOLDER));
    let expected = [("/folder/b", name.clone()), ("/folder/c", Reason::NotAllowed(Some("no rights".to_owned())).to_string()), ("/folder/z", name.clone())];
    assert_eq!(listed, expected.map(|(path, reason)| KeptBack { path: path.to_owned(), reason }));

    let of_reason: Vec<LocalItemId> = list_entries_kept_back_for(&entries, &running(None), Reason::NameCharacters.key()).iter().map(|entry| entry.item.local_item_id).collect();
    assert_eq!(of_reason, [id(11), id(12)], "by local item id");
    let files = list_kept_back_files(&entries, &paths, &running(None), Reason::NameCharacters.key(), 0, Path::new(FOLDER));
    assert_eq!(files, KeptBackFiles { items: vec![KeptBack { path: "/folder/b".into(), reason: name.clone() }, KeptBack { path: "/folder/z".into(), reason: name.clone() }], total: 2 });
    // The first of them by item, and how many there are whatever the limit.
    let files = list_kept_back_files(&entries, &paths, &running(None), Reason::NameCharacters.key(), 1, Path::new(FOLDER));
    assert_eq!(files, KeptBackFiles { items: vec![KeptBack { path: "/folder/z".into(), reason: name }], total: 2 });
    // An entry with no path is left out of the list and stays in the count.
    let files = list_kept_back_files(&entries, &paths, &running(None), Reason::TooBig(None).key(), 0, Path::new(FOLDER));
    assert_eq!(files, KeptBackFiles { items: Vec::new(), total: 1 });
}

// ---- the counts ----

#[test]
fn the_counts_are_the_stores_sum_less_what_the_memory_and_the_answers_take_out() {
    let numbered = |number: i64, item: Item, answer: Option<ItemAnswer>| {
        let mut item = item;
        item.local_item_id = id(number);
        entry(item, answer)
    };
    let mut no_kind = numbered(8, agreed_file(), for_user(Reason::NameCharacters));
    no_kind.item.unsent_since = Some(9);
    let answered = vec![
        numbered(2, removed_file(), Some(ItemAnswer::Planned(StepClass::Metadata))),
        numbered(3, changed_file(), for_user(Reason::NameCharacters)),
        numbered(4, changed_file(), Some(ItemAnswer::Excluded(ExclusionReason::CannotBeShown))),
        numbered(5, changed_file(), for_user(Reason::WaitingForSpace)),
        numbered(6, changed_file(), Some(ItemAnswer::Planned(StepClass::Content))),
        numbered(7, changed_file(), for_user(Reason::TooBig(Some((20, 5))))),
        // A stamp and no kind: no entry, and it stays pending.
        no_kind,
    ];
    // In the store: the seven above, one item the memory refuses for good that the plan
    // was not given, and eight more.
    let totals = QueueTotals { unsent: UnsentTotal { count: 16, bytes: 1_000 }, refused_for_good: vec![id(40)], refused_for_space: vec![(id(7), 20)], answered };
    let counts = count_queue(&totals, &running(None));
    assert_eq!(counts, QueueCounts { pending: 16 - 2 - 1, pending_bytes: 1_000 - 20 - 20, blocked: 2, space_waiting: 1, space_waiting_bytes: 20, too_big: 1, too_big_bytes: 20 });
}

#[test]
fn an_item_the_memory_and_the_answers_both_refuse_is_counted_once() {
    let mut refused = entry(changed_file(), for_user(Reason::Forbidden));
    refused.memory.refusal = Some(refusal(Reason::Forbidden));
    let totals = QueueTotals { unsent: UnsentTotal { count: 3, bytes: 60 }, refused_for_good: vec![id(7)], refused_for_space: Vec::new(), answered: vec![refused] };
    let counts = count_queue(&totals, &running(None));
    assert_eq!((counts.blocked, counts.pending, counts.pending_bytes), (1, 2, 40));
    // Nothing goes below nothing.
    let none = QueueTotals { unsent: UnsentTotal::default(), ..totals };
    assert_eq!((count_queue(&none, &running(None)).pending, count_queue(&none, &running(None)).pending_bytes), (0, 0));
}

// ---- the uploads, what is not shown ----

#[test]
fn a_running_upload_is_shown_under_its_items_path() {
    let steps = [step(id(3), StepClass::Content, 5, 50), step(id(4), StepClass::Metadata, 0, 0), step(id(5), StepClass::Download, 1, 2), step(id(6), StepClass::Content, 0, 9)];
    let paths = BTreeMap::from([(id(3), PathBuf::from("docs/a")), (id(4), PathBuf::from("b")), (id(5), PathBuf::from("c"))]);
    let uploads = describe_running_uploads(&steps, &paths, Path::new(FOLDER));
    assert_eq!(uploads, [Transfer { path: "/folder/docs/a".into(), done: 5, total: 50 }], "a step that sends content and has a path");
}

#[test]
fn an_item_not_shown_that_is_still_here_says_its_path_and_what_keeps_it() {
    let mut item = agreed_file();
    item.placement = Placement::Skipped(SkipReason::NameTooLong);
    let not_shown = ItemNotShown { item, onedrive_path: PathBuf::from("docs/long"), reason: SkipReason::NameTooLong };
    let row = |local_path: Option<&str>, answer: Option<ItemAnswer>| describe_item_not_shown(&not_shown, local_path.map(Path::new), answer.as_ref(), Path::new(FOLDER));
    let gone = row(None, for_user(Reason::OpenForWriting));
    assert_eq!(gone, NotInFolder { path: "/folder/docs/long".into(), reason: "name-too-long".into(), waits: String::new(), here: String::new() });
    let cases: Vec<(Option<ItemAnswer>, &str)> = vec![
        (for_user(Reason::OpenForWriting), "open-for-writing:/folder/docs/here"),
        (None, "cycle"),
        (waiting(WaitReason::Listing), "cycle"),
        (Some(ItemAnswer::Planned(StepClass::Folder)), "cycle"),
        (Some(ItemAnswer::Excluded(ExclusionReason::CannotBeShown)), "cycle"),
        (Some(ItemAnswer::Fault(PlanFault::RowAgainstRules(id(7)))), "cycle"),
        (waiting(WaitReason::AnotherItem(id(2))), "uploads:1"),
        (for_user(Reason::Parent), "uploads:1"),
    ];
    for (answer, waits) in cases {
        let here = row(Some("docs/here"), answer.clone());
        assert_eq!((here.here.as_str(), here.waits.as_str()), ("/folder/docs/here", waits), "{answer:?}");
    }
}

// ---- the readers, over a store ----

/// A folder with an unsent item in every state, and what the last plan answered for
/// each. The rows come to be unsent in the order they are made in.
struct QueueFixture {
    fixture: ItemFixture,
    old_a: LocalItemId,
    old: LocalItemId,
    uploading: LocalItemId,
    gone: LocalItemId,
    refused: LocalItemId,
    too_big: LocalItemId,
    left_out: LocalItemId,
    answers: BTreeMap<LocalItemId, ItemAnswer>,
    running: Vec<RunningStep>,
}

const UNSENT_ROWS: u64 = 16;

impl QueueFixture {
    fn new() -> Self {
        let fixture = ItemFixture::new();
        let docs = fixture.show_onedrive_directory("docs").local_item_id;
        let old = fixture.show_onedrive_directory("old").local_item_id;
        let old_a = fixture.show_onedrive_file("old/a.txt", b"a").local_item_id;
        let old_b = fixture.show_onedrive_file("old/b.txt", b"b").local_item_id;
        let moved = fixture.show_onedrive_file("docs/moved.txt", b"moved").local_item_id;
        let changed = fixture.show_onedrive_file("docs/changed.txt", b"before").local_item_id;
        fixture.land_copy(changed, b"before");
        let gone = fixture.show_onedrive_file("docs/gone.txt", b"gone").local_item_id;

        fixture.remove_here(old_a).expect("a removal to send");
        fixture.remove_here(old_b).expect("a removal to send");
        fixture.remove_here(old).expect("a removal to send");
        let created = fixture.make_own_file("docs/created.txt", b"12345").local_item_id;
        let uploading = fixture.make_own_file("docs/uploading.bin", b"0123456789").local_item_id;
        fixture.move_here(moved, "docs/moved-here.txt");
        let changed_now = fixture.change_file_here(changed, b"after!!");
        fixture.remove_here(gone).expect("a removal to send");
        let refused = fixture.make_own_file("docs/refused.txt", b"xyz");
        let too_big = fixture.make_own_file("docs/too-big.bin", &[0; 64]);
        let left_out = fixture.make_own_file("docs/left-out.txt", b"abcdef").local_item_id;
        let faulty = fixture.make_own_file("docs/faulty.txt", b"f").local_item_id;
        let unread = fixture.make_own_file("docs/unread.txt", b"u").local_item_id;
        let made_directory = fixture.make_own_directory("docs/made").local_item_id;
        let spaced = fixture.make_own_file("docs/spaced.txt", b"1234").local_item_id;
        let named = fixture.make_own_file("docs/named.txt", b"12").local_item_id;

        let (refused_id, too_big_id) = (refused.local_item_id, too_big.local_item_id);
        fixture.write_rows(move |store| {
            let mut memory = store.step_memory();
            let refusal = Refusal { reason: Reason::Refused(Some("bad".to_owned())), version: RefusedVersion::Shown(refused.local.clone().expect("shown")) };
            assert_eq!(memory.remember_refusal(refused.local_item_id, refused.unsent_since, &refusal)?, FactStanding::StillSaid);
            let space_refusal = SpaceRefusal { content: too_big.local.as_ref().and_then(|local| local.content.clone()).expect("a content"), free_space: 10 };
            assert_eq!(memory.remember_space_refusal(too_big.local_item_id, too_big.unsent_since, &space_refusal)?, FactStanding::StillSaid);
            assert_eq!(memory.remember_failed_try(changed, changed_now.unsent_since, &Reason::Network, Some(START_TIME + 60))?, FailedTryStanding::Counted(1));
            Ok(())
        });

        let answers = BTreeMap::from([
            (old, ItemAnswer::Planned(StepClass::Metadata)),
            (old_a, ItemAnswer::Waiting(WaitReason::AnotherItem(old))),
            (old_b, ItemAnswer::Waiting(WaitReason::AnotherItem(old))),
            (created, ItemAnswer::Planned(StepClass::Content)),
            (uploading, ItemAnswer::Waiting(WaitReason::RunningStep)),
            (moved, ItemAnswer::Waiting(WaitReason::AnotherItem(docs))),
            (changed, ItemAnswer::Waiting(WaitReason::Time { until: START_TIME + 60, reason: Reason::Network })),
            (gone, ItemAnswer::Planned(StepClass::Metadata)),
            (left_out, ItemAnswer::Excluded(ExclusionReason::CannotBeShown)),
            (faulty, ItemAnswer::Fault(PlanFault::RowAgainstRules(faulty))),
            (unread, ItemAnswer::Waiting(WaitReason::StartUpCheck)),
            (made_directory, ItemAnswer::Waiting(WaitReason::Listing)),
            (spaced, ItemAnswer::Waiting(WaitReason::User(Reason::WaitingForSpace))),
            (named, ItemAnswer::Waiting(WaitReason::User(Reason::NameCharacters))),
        ]);
        let running = vec![step(uploading, StepClass::Content, 4, 10)];
        QueueFixture { fixture, old_a, old, uploading, gone, refused: refused_id, too_big: too_big_id, left_out, answers, running }
    }

    /// `Changes(limit)`, as the owner of the folder's state answers it.
    fn read_changes(&self, sending: &QueueConditions, limit: usize) -> Vec<Change> {
        let (answers, running, sending) = (self.answers.clone(), self.running.clone(), sending.clone());
        self.fixture.read_rows(move |items| {
            let entries = read_unsent_entries(items, &answers, &running, limit)?;
            let paths = read_entry_paths(items, &entries.iter().collect::<Vec<_>>())?;
            Ok(entries.iter().filter_map(|entry| describe_queue_change(entry, paths.get(&entry.item.local_item_id)?, &sending, Path::new(FOLDER))).collect())
        })
    }

    fn read_counts(&self, sending: &QueueConditions) -> (QueueTotals, QueueCounts) {
        let (answers, running) = (self.answers.clone(), self.running.clone());
        let totals = self.fixture.read_rows(move |items| read_queue_totals(items, &answers, &running));
        let counts = count_queue(&totals, sending);
        (totals, counts)
    }

    fn read_kept_back(&self) -> Vec<UnsentEntry> {
        let (answers, running) = (self.answers.clone(), self.running.clone());
        self.fixture.read_rows(move |items| read_kept_back_entries(items, &answers, &running))
    }

    fn read_paths(&self, entries: Vec<UnsentEntry>) -> BTreeMap<LocalItemId, PathBuf> {
        self.fixture.read_rows(move |items| read_entry_paths(items, &entries.iter().collect::<Vec<_>>()))
    }

    fn conditions(&self) -> QueueConditions {
        self.fixture.sending_holds.set_listing_ended();
        read_queue_conditions(&self.fixture.sending_holds, &self.fixture.onedrive_space, START_TIME)
    }
}

/// The rows of `changes` as (path below the folder, kind, state, reason).
fn words(changes: &[Change]) -> Vec<(String, String, String, String)> {
    changes.iter().map(|change| (change.path.trim_start_matches("/folder/").to_owned(), change.kind.clone(), change.state.clone(), change.reason.clone())).collect()
}

fn word(path: &str, kind: &str, state: &str, reason: &str) -> (String, String, String, String) {
    (path.to_owned(), kind.to_owned(), state.to_owned(), reason.to_owned())
}

#[test]
fn the_conditions_are_the_hold_shown_and_whether_onedrive_is_full() {
    let fixture = ItemFixture::new();
    let read = || read_queue_conditions(&fixture.sending_holds, &fixture.onedrive_space, START_TIME);
    assert_eq!(read(), running(Some(SendingHold::FirstListingNotEnded)));
    fixture.sending_holds.set_listing_ended();
    assert_eq!(read(), running(None));
    fixture.sending_holds.set_paused(Some(None));
    assert_eq!(read(), running(Some(SendingHold::Paused { until: None })));
}

#[test]
fn changes_lists_every_unsent_item_once_in_its_state_oldest_first() {
    let queue = QueueFixture::new();
    let changes = queue.read_changes(&queue.conditions(), 0);
    let expected = vec![
        // What was removed with a directory waits for the directory's removal.
        word("old/a.txt", "delete", "waiting", ""),
        word("old/b.txt", "delete", "waiting", ""),
        word("old", "delete", "ready", ""),
        word("docs/created.txt", "create", "ready", ""),
        word("docs/uploading.bin", "create", "running", ""),
        word("docs/moved-here.txt", "move", "waiting", ""),
        word("docs/changed.txt", "update", "retry", "network"),
        // A removal is listed under the path of its synced place.
        word("docs/gone.txt", "delete", "ready", ""),
        word("docs/refused.txt", "create", "blocked", "refused: bad"),
        word("docs/too-big.bin", "create", "ready", "too-big:64:10"),
        // `docs/left-out.txt`, which the plan leaves out, is no row.
        word("docs/faulty.txt", "create", "waiting", "being-checked"),
        word("docs/unread.txt", "create", "waiting", "not-read-on-disk"),
        word("docs/made", "mkdir", "waiting", ""),
        word("docs/spaced.txt", "create", "ready", "waiting-for-space"),
        word("docs/named.txt", "create", "blocked", "name-characters"),
    ];
    assert_eq!(words(&changes), expected);
    let by_path = |path: &str| changes.iter().find(|change| change.path == format!("/folder/{path}")).expect("a row");
    assert_eq!((by_path("docs/uploading.bin").sent, by_path("docs/uploading.bin").total), (4, 10));
    assert_eq!((by_path("docs/created.txt").sent, by_path("docs/created.txt").total), (0, 5));
    assert_eq!(by_path("docs/changed.txt").next_try, START_TIME + 60);
    let mut stamps: Vec<u64> = changes.iter().map(|change| change.seq).collect();
    assert!(stamps.windows(2).all(|pair| pair[0] < pair[1]), "in the order they came to be unsent, each once: {stamps:?}");
    stamps.dedup();
    assert_eq!(stamps.len(), changes.len());
}

#[test]
fn the_counts_agree_with_the_rows_of_changes() {
    let queue = QueueFixture::new();
    let sending = queue.conditions();
    let (totals, counts) = queue.read_counts(&sending);
    assert_eq!(totals.unsent.count, UNSENT_ROWS);
    assert_eq!(totals.refused_for_good, [queue.refused]);
    assert_eq!(totals.refused_for_space, [(queue.too_big, 64)]);
    let answered: Vec<LocalItemId> = totals.answered.iter().map(|entry| entry.item.local_item_id).collect();
    assert!(answered.contains(&queue.old) && answered.contains(&queue.old_a) && answered.contains(&queue.left_out));

    let changes = queue.read_changes(&sending, 0);
    let count_of = |state: &str| changes.iter().filter(|change| change.state == state).count() as u32;
    assert_eq!(counts.blocked, count_of("blocked"));
    assert_eq!(counts.pending, changes.len() as u32 - count_of("blocked"), "what is pending is every row that is not blocked");
    assert_eq!((counts.blocked, counts.pending), (2, 13));
    assert_eq!((counts.too_big, counts.too_big_bytes, counts.space_waiting, counts.space_waiting_bytes), (1, 64, 1, 4));
    // Off the bytes: the file the plan blocks (2 bytes) and the one it leaves out (6).
    assert_eq!(counts.pending_bytes, totals.unsent.bytes - 2 - 6);
}

#[test]
fn what_is_kept_back_agrees_with_the_rows_and_the_counts() {
    let queue = QueueFixture::new();
    let sending = queue.conditions();
    let entries = queue.read_kept_back();
    let mut items: Vec<LocalItemId> = entries.iter().map(|entry| entry.item.local_item_id).collect();
    assert!(items.contains(&queue.refused) && items.contains(&queue.too_big), "what the memory refuses, though the plan was not given it");
    let listed = items.len();
    items.dedup();
    assert_eq!(items.len(), listed, "each item once");

    let summary = summarize_kept_back(&entries, &sending);
    let expected = vec![
        summary_row("one-action", &Reason::TooBig(None), 1, 64),
        summary_row("one-action", &Reason::WaitingForSpace, 1, 4),
        summary_row("per-file", &Reason::NameCharacters, 1, 2),
        // Not read on disk: the reason's own group is `per-file`, though its row reads `waiting`.
        summary_row("per-file", &Reason::NotReadOnDisk, 1, 1),
        summary_row("per-file", &Reason::Refused(None), 1, 3),
        summary_row("waiting", &Reason::BeingChecked, 1, 1),
        summary_row("waiting", &Reason::Network, 1, 7),
    ];
    assert_eq!(summary, expected);
    let (_, counts) = queue.read_counts(&sending);
    assert_eq!(list_blocked_entries(&entries, &sending).len() as u32, counts.blocked);

    let paths = queue.read_paths(entries.clone());
    let not_uploaded = list_not_uploaded(&entries, &paths, &sending, Path::new(FOLDER));
    assert_eq!(not_uploaded, [KeptBack { path: "/folder/docs/named.txt".into(), reason: "name-characters".into() }, KeptBack { path: "/folder/docs/refused.txt".into(), reason: "refused: bad".into() }]);
    let too_big = list_kept_back_files(&entries, &paths, &sending, Reason::TooBig(None).key(), 0, Path::new(FOLDER));
    assert_eq!(too_big, KeptBackFiles { items: vec![KeptBack { path: "/folder/docs/too-big.bin".into(), reason: "too-big:64:10".into() }], total: 1 });
}

#[test]
fn a_pause_is_the_word_of_a_row_and_nothing_else() {
    let queue = QueueFixture::new();
    let sending = queue.conditions();
    queue.fixture.sending_holds.set_paused(Some(None));
    let paused = queue.conditions();
    assert_eq!(paused.hold, Some(SendingHold::Paused { until: None }));

    let changes = queue.read_changes(&paused, 0);
    let states: Vec<(&str, &str)> = changes.iter().map(|change| (change.state.as_str(), change.reason.as_str())).collect();
    let kept: Vec<(&str, &str)> = states.iter().copied().filter(|(state, _)| *state != "paused").collect();
    assert_eq!(kept, [("blocked", "refused: bad"), ("waiting", "being-checked"), ("waiting", "not-read-on-disk"), ("blocked", "name-characters")]);
    assert_eq!(states.iter().filter(|(state, reason)| *state == "paused" && reason.is_empty()).count(), 11);
    let uploading = changes.iter().find(|change| change.path.ends_with("uploading.bin")).expect("a row");
    assert_eq!((uploading.state.as_str(), uploading.sent, uploading.total), ("paused", 4, 10), "the bytes stay");

    assert_eq!(queue.read_counts(&paused).1, queue.read_counts(&sending).1);
    let entries = queue.read_kept_back();
    assert_eq!(summarize_kept_back(&entries, &paused), summarize_kept_back(&entries, &sending));
}

#[test]
fn with_no_running_sync_every_unsent_item_waits_but_what_the_memory_refuses() {
    let queue = QueueFixture::new();
    let stopped = describe_stopped_sync();
    let entries = queue.fixture.read_rows(|items| read_unsent_entries(items, &BTreeMap::new(), &[], 0));
    assert_eq!(entries.len() as u64, UNSENT_ROWS);
    let paths = queue.read_paths(entries.clone());
    let changes: Vec<Change> = entries.iter().filter_map(|entry| describe_queue_change(entry, paths.get(&entry.item.local_item_id)?, &stopped, Path::new(FOLDER))).collect();
    assert_eq!(changes.len() as u64, UNSENT_ROWS, "every row has a path: a removal that of its synced place, below a removed directory too");
    assert!(changes.iter().any(|change| change.path == "/folder/old/a.txt" && change.kind == "delete"));
    let not_waiting: Vec<(&str, &str)> = changes.iter().filter(|change| change.state != "waiting").map(|change| (change.path.as_str(), change.state.as_str())).collect();
    assert_eq!(not_waiting, [("/folder/docs/refused.txt", "blocked")]);
}

#[test]
fn an_entry_with_no_path_is_in_no_row() {
    let queue = QueueFixture::new();
    // An item with no row: neither a local path nor a synced one.
    let mut no_row = entry(removed_file(), None);
    no_row.item.local_item_id = id(9_999);
    assert!(queue.read_paths(vec![no_row]).is_empty());
}

#[test]
fn the_uploads_are_the_running_steps_that_send_content_under_their_paths() {
    let queue = QueueFixture::new();
    let mut running = queue.running.clone();
    running.push(step(queue.gone, StepClass::Metadata, 0, 0));
    let steps = running.clone();
    let paths = queue.fixture.read_rows(move |items| read_running_upload_paths(items, &steps));
    assert_eq!(paths, BTreeMap::from([(queue.uploading, PathBuf::from("docs/uploading.bin"))]));
    assert_eq!(describe_running_uploads(&running, &paths, Path::new(FOLDER)), [Transfer { path: "/folder/docs/uploading.bin".into(), done: 4, total: 10 }]);
}

#[test]
fn a_folder_that_shows_all_of_onedrive_has_no_row_of_what_is_not_shown() {
    let queue = QueueFixture::new();
    let answers = queue.answers.clone();
    assert_eq!(queue.fixture.read_rows(move |items| read_items_not_shown(items, &answers, Path::new(FOLDER))), Vec::new());
}

// ---- the properties ----

#[test]
fn the_properties_are_written_by_the_one_writer_and_emptied_at_a_stop() {
    let state = SyncStateHandle::new(SyncSnapshot::default());
    state.update(|snapshot| snapshot.upload_queue.note = Some(UploadQueueNote::Throttled(77)));
    let counts = QueueCounts { pending: 9, pending_bytes: 900, blocked: 2, space_waiting: 3, space_waiting_bytes: 30, too_big: 4, too_big_bytes: 40 };
    publish_queue_status(&state, counts, vec![Transfer { path: "/folder/a".into(), done: 1, total: 2 }], true);
    let upload_queue = state.get().upload_queue;
    assert_eq!((upload_queue.pending_count, upload_queue.pending_bytes, upload_queue.blocked_count), (9, 900, 2));
    assert_eq!((upload_queue.space_waiting_count, upload_queue.space_waiting_bytes, upload_queue.too_big_count, upload_queue.too_big_bytes), (3, 30, 4, 40));
    assert_eq!(upload_queue.uploads, [("/folder/a".to_owned(), 1, 2)]);
    assert!(upload_queue.quota_full);
    assert_eq!(upload_queue.note, Some(UploadQueueNote::Throttled(77)), "the note is the plan's task's");

    publish_stopped_queue(&state);
    assert_eq!(state.get().upload_queue, SyncSnapshot::default().upload_queue, "every count 0, no upload, not full, no note");
}
