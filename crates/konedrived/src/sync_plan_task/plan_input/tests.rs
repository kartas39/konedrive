//! What a plan is made from, over the mount's fixture: the store's rows read through the
//! plan's own closure.

use konedrive_tree::{FactStanding, Refusal, RefusedVersion};

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
        settings: PlanSettings { computer_name: "here".into(), largest_file_size: 1 << 30, quiet_time: 0, items_read_at_once: 100 },
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
