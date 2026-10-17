//! What a plan is made from, over the mount's fixture: the store's rows read through the
//! plan's own closure.

use konedrive_tree::{FactStanding, Refusal, RefusedVersion, SyncStep};

use super::*;
use crate::mount::item_fixture::{ItemFixture, START_TIME};

fn facts() -> DaemonFacts {
    DaemonFacts {
        items_with_running_step: BTreeSet::new(),
        destinations_of_running_steps: Vec::new(),
        is_onedrive_full: false,
        now: START_TIME + 1_000,
        left_by_start_up_check: BTreeSet::new(),
        local_step_tries: LocalStepTries::default(),
        is_pinned_files_reader_asked: true,
        settings: PlanSettings { computer_name: "here".into(), quiet_time: 0, items_read_at_once: 100 },
    }
}

fn plan(fixture: &ItemFixture, facts: DaemonFacts) -> PlannedRound {
    fixture.read_rows(move |items| plan_next_steps(items, &facts))
}

/// OneDrive refuses `item` for good as it is.
fn refuse_for_good(fixture: &ItemFixture, item: &Item) {
    let (id, stamp, local) = (item.local_item_id, item.unsent_since, item.local.clone().expect("shown"));
    fixture.write_rows(move |store| {
        let refusal = Refusal { reason: Reason::Forbidden, version: RefusedVersion::Shown(local) };
        assert_eq!(store.step_memory().remember_refusal(id, stamp, &refusal)?, FactStanding::StillSaid);
        Ok(())
    });
}

#[test]
fn the_unsent_that_will_not_be_sent_by_themselves_are_the_refused_for_good_those_the_check_left_and_what_waits_for_them() {
    let fixture = ItemFixture::new();
    let sent = fixture.make_own_file("sent.txt", b"1");
    let refused = fixture.make_own_directory("refused");
    let in_refused = fixture.make_own_file("refused/in.txt", b"2");
    let left = fixture.make_own_file("left.txt", b"3");
    refuse_for_good(&fixture, &refused);

    let round = plan(&fixture, DaemonFacts { left_by_start_up_check: [left.local_item_id].into(), ..facts() });
    let wait_of = |item: LocalItemId| round.plan.waits.iter().find(|wait| wait.item == item).map(|wait| wait.reason.clone());
    assert_eq!(wait_of(refused.local_item_id), Some(WaitReason::User(Reason::Forbidden)));
    assert_eq!(wait_of(in_refused.local_item_id), Some(WaitReason::AnotherItem(refused.local_item_id)));
    assert_eq!(wait_of(left.local_item_id), Some(WaitReason::StartUpCheck));
    assert_eq!(wait_of(sent.local_item_id), None);
    assert_eq!(count_unsent_not_to_send(&round.input, &round.plan, &round.memories), 3);
    assert_eq!(round.unsent_to_send_count, 1, "of the four unsent, one will be sent");

    // A wait for the user that the item's own change ends, and a wait for a time, are of what will be sent.
    let mut waits_only = round.plan.clone();
    waits_only.waits = vec![
        konedrive_tree::ItemWait { item: sent.local_item_id, reason: WaitReason::User(Reason::OpenForWriting) },
        konedrive_tree::ItemWait { item: left.local_item_id, reason: WaitReason::Time { until: START_TIME + 2_000, reason: Reason::Network } },
    ];
    assert_eq!(count_unsent_not_to_send(&round.input, &waits_only, &round.memories), 0);
}

/// The owner's rule for a kept wish for read-only: it waits for what will be sent, and
/// never for what will not be sent by itself.
#[test]
fn the_count_the_account_waits_for_leaves_out_what_will_not_be_sent_by_itself() {
    let fixture = ItemFixture::new();
    let refused = fixture.make_own_file("refused.txt", b"1");
    let left = fixture.make_own_file("left.txt", b"2");
    assert_eq!(plan(&fixture, facts()).unsent_to_send_count, 2);
    refuse_for_good(&fixture, &refused);
    assert_eq!(plan(&fixture, facts()).unsent_to_send_count, 1);
    let round = plan(&fixture, DaemonFacts { left_by_start_up_check: [left.local_item_id].into(), ..facts() });
    assert_eq!(round.unsent_to_send_count, 0, "nothing is left that a switch to read-only would wait for");
    assert_eq!(fixture.read_rows(|items| items.count_unsent()).count, 2, "every unsent change is counted for the warning before a Forget");
}

#[test]
fn each_case_of_a_reading_is_enough_or_reads_more() {
    let window = ReadWindow { unsent: 4, of_each_other_kind: 4 };
    let reading = WindowReading { unsent_read: 4, is_unsent_read_whole: false, unsent_waiting_for_user: 0, is_all_read: false, has_step: true };
    assert_eq!(decide_window(4, window, reading), WindowDecision::Enough, "a window of unsent items that can have a step");
    assert_eq!(decide_window(4, window, WindowReading { is_all_read: true, has_step: false, unsent_waiting_for_user: 4, ..reading }), WindowDecision::Enough, "everything was read");
    assert_eq!(decide_window(4, window, WindowReading { has_step: false, ..reading }), WindowDecision::ReadMore(ReadWindow { unsent: 8, of_each_other_kind: 8 }), "no step at all, and more is there");
    assert_eq!(decide_window(4, window, WindowReading { unsent_waiting_for_user: 1, ..reading }), WindowDecision::ReadMore(ReadWindow { unsent: 8, of_each_other_kind: 4 }), "one of the window waits for the user: the window moves on");
    let wider = ReadWindow { unsent: 8, of_each_other_kind: 4 };
    assert_eq!(decide_window(4, wider, WindowReading { unsent_read: 8, unsent_waiting_for_user: 4, ..reading }), WindowDecision::Enough, "four that can have a step are in it");
    assert_eq!(decide_window(4, wider, WindowReading { unsent_read: 6, is_unsent_read_whole: true, unsent_waiting_for_user: 5, ..reading }), WindowDecision::Enough, "no more unsent items are there");
}

/// A window full of unsent items that cannot be sent, a step of another
/// kind planned, and a newer unsent item beyond the window.
#[test]
fn unsent_items_that_wait_for_the_user_do_not_hold_the_window_while_another_step_is_planned() {
    let fixture = ItemFixture::new();
    let refused: Vec<Item> = (0..3).map(|number| fixture.make_own_file(&format!("refused-{number}.txt"), b"1")).collect();
    for item in &refused {
        refuse_for_good(&fixture, item);
    }
    let newer = fixture.make_own_file("newer.txt", b"2");
    let listed = fixture.list_onedrive_file("theirs.txt", b"3");
    let mut facts = facts();
    facts.settings.items_read_at_once = 2;

    let round = plan(&fixture, facts);
    let step_of = |item: LocalItemId| round.plan.steps.iter().find(|step| step.local_item_id() == item).cloned();
    assert!(matches!(step_of(listed.local_item_id), Some(SyncStep::Folder(_))), "the step that kept the window from being widened: {:?}", round.plan);
    assert!(matches!(step_of(newer.local_item_id), Some(SyncStep::OneDrive(_))), "the newer unsent item is planned past those that wait for the user: {:?}", round.plan);
    assert_eq!(count_unsent_waiting_for_user(&refused.iter().map(|item| item.local_item_id).chain([newer.local_item_id]).collect::<Vec<_>>(), &round.plan), 3);
}

#[test]
fn the_pinned_to_download_are_read_while_the_reader_is_asked_and_not_otherwise() {
    let fixture = ItemFixture::new();
    assert_eq!(plan(&fixture, facts()).pinned_files_reading, PinnedFilesReading::NoneMissing, "no pin");
    let pinned = fixture.show_onedrive_file("pinned.txt", b"theirs");
    let id = pinned.local_item_id;
    let onedrive_id = pinned.onedrive_item.as_ref().expect("bound").item_id.as_str().to_owned();
    fixture.write_rows(move |store| store.set_pin(&onedrive_id, true));

    let asked = plan(&fixture, facts());
    assert_eq!(asked.pinned_files_reading, PinnedFilesReading::SomeMissing(PinnedDownloadTotal { count: 1, bytes: 6 }), "with all that is missing, counted");
    assert_eq!(asked.pinned_files_reading.find_total(), Some(PinnedDownloadTotal { count: 1, bytes: 6 }));
    assert_eq!(PinnedFilesReading::NoneMissing.find_total(), Some(PinnedDownloadTotal::default()));
    assert!(asked.plan.steps.iter().any(|step| matches!(step, SyncStep::Folder(konedrive_tree::FolderStep::DownloadContent { item }) if item.local_item_id == id)), "{:?}", asked.plan);

    let not_asked = plan(&fixture, DaemonFacts { is_pinned_files_reader_asked: false, ..facts() });
    assert_eq!(not_asked.pinned_files_reading, PinnedFilesReading::NotAsked);
    assert_eq!(not_asked.pinned_files_reading.find_total(), None, "nothing is known anew");
    assert_eq!(not_asked.plan.steps, Vec::new(), "the file agrees on everything, and nothing else gives it to a plan");
}
