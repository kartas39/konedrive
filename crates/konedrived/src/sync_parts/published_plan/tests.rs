use super::*;

fn item(number: i64) -> LocalItemId {
    LocalItemId::new(number)
}

#[test]
fn what_was_published_is_read_back_per_item() {
    let published_plan = PublishedPlan::new();
    assert_eq!(published_plan.find_answer(item(5)), None, "before the first plan");
    assert!(published_plan.list_answers().is_empty());
    let answers = BTreeMap::from([(item(5), ItemAnswer::Planned(StepClass::Content)), (item(6), ItemAnswer::Excluded(ExclusionReason::NameIgnored))]);
    published_plan.publish(answers.clone());
    assert_eq!(published_plan.find_answer(item(5)), Some(ItemAnswer::Planned(StepClass::Content)));
    assert_eq!(published_plan.find_answer(item(6)), Some(ItemAnswer::Excluded(ExclusionReason::NameIgnored)));
    assert_eq!(published_plan.find_answer(item(7)), None, "an item the plan was not given");
    assert_eq!(published_plan.list_answers(), answers);
}

#[test]
fn a_new_plan_replaces_the_answers_of_the_one_before() {
    let published_plan = PublishedPlan::new();
    published_plan.publish(BTreeMap::from([(item(5), ItemAnswer::Planned(StepClass::Content)), (item(6), ItemAnswer::Planned(StepClass::Metadata))]));
    let next = BTreeMap::from([(item(6), ItemAnswer::Excluded(ExclusionReason::CannotBeShown))]);
    published_plan.publish(next.clone());
    assert_eq!(published_plan.find_answer(item(5)), None, "the item is not in the new plan: its answer is gone");
    assert_eq!(published_plan.list_answers(), next);
}

/// The one rule of "blocked": a case per answer, per wait and per group of a reason.
#[test]
fn only_a_refusal_for_good_and_a_wait_for_the_user_that_needs_the_user_are_shown_blocked() {
    let other = item(8);
    let waiting = |reason: WaitReason| Some(ItemAnswer::Waiting(reason));
    let user = |reason: Reason| waiting(WaitReason::User(reason));
    // A reason of each group, as the test relies on them.
    let in_no_table = Reason::parse("a reason no table has");
    assert_eq!(
        [Reason::Forbidden.group(), Reason::NameCharacters.group(), Reason::Locked.group(), in_no_table.group()],
        [Some(Group::OneAction), Some(Group::PerFile), Some(Group::Waiting), None]
    );
    assert_eq!((Reason::NotReadOnDisk.group(), Reason::BeingChecked.group()), (Some(Group::PerFile), Some(Group::Waiting)), "the two the owner decided come before their groups");
    let cases: Vec<(Option<ItemAnswer>, bool, &str)> = vec![
        (None, false, "an item the last plan was not given"),
        (Some(ItemAnswer::Planned(StepClass::Content)), false, "a planned step"),
        (Some(ItemAnswer::Planned(StepClass::Folder)), false, "a planned step towards the folder"),
        (Some(ItemAnswer::Excluded(ExclusionReason::NameIgnored)), false, "left here"),
        (Some(ItemAnswer::Excluded(ExclusionReason::CannotBeShown)), false, "cannot be shown"),
        (Some(ItemAnswer::Excluded(ExclusionReason::LeftByUpload)), false, "left by an upload"),
        (Some(ItemAnswer::Fault(PlanFault::RowAgainstRules(other))), false, "a fault: being checked"),
        (Some(ItemAnswer::Fault(PlanFault::FullListingNeeded(other))), false, "a fault that asks a full listing"),
        (Some(ItemAnswer::Fault(PlanFault::WaitWithoutEnd(other))), false, "a fault: a wait without end"),
        (Some(ItemAnswer::Fault(PlanFault::NoFreeName(other))), false, "a fault: no free name"),
        (waiting(WaitReason::AnotherItem(other)), false, "a wait for another item"),
        (waiting(WaitReason::ItemNotGiven(other)), false, "a wait for an item not given"),
        (waiting(WaitReason::Time { until: 5, reason: Reason::Network }), false, "a wait for a time"),
        (waiting(WaitReason::Time { until: 5, reason: Reason::Forbidden }), false, "a wait for a time, whatever its reason"),
        (waiting(WaitReason::RunningStep), false, "a running step"),
        (waiting(WaitReason::StartUpCheck), false, "left by the check at start"),
        (waiting(WaitReason::Listing), false, "a wait for a listing"),
        (waiting(WaitReason::ItemsInDirectoryNotGiven(other)), false, "a directory not given whole"),
        // A wait for the user, by its reason.
        (user(Reason::NotReadOnDisk), false, "could not be read on disk"),
        (user(Reason::BeingChecked), false, "being checked"),
        (user(Reason::WaitingForSpace), false, "a wait for space"),
        (user(Reason::TooBig(Some((300, 20)))), false, "too big for the space left: a wait for space"),
        (user(Reason::OpenForWriting), false, "open for writing: of the group waiting"),
        (user(Reason::NotAllowed(Some(konedrive_tree::READ_ONLY_FOLDER.to_owned()))), false, "a read-only folder's wait"),
        (user(Reason::Forbidden), true, "one action: the sign-in"),
        (user(Reason::TooBig(None)), true, "one action, and no wait for space"),
        (user(Reason::NameCharacters), true, "per file: a name"),
        (user(Reason::Refused(Some("no".into()))), true, "per file: refused with a message"),
        (user(in_no_table), true, "a reason in no table"),
    ];
    for (answer, is_blocked, what) in &cases {
        assert_eq!(decide_is_shown_blocked(answer.as_ref(), false), *is_blocked, "{what}");
        assert!(decide_is_shown_blocked(answer.as_ref(), true), "a refusal for good the memory keeps is blocked whatever the plan answered: {what}");
    }
}

#[test]
fn the_count_of_what_will_not_be_sent_is_the_last_plans() {
    let published = PublishedPlan::new();
    assert_eq!(published.count_unsent_not_to_send(), 0, "before the first plan");
    published.publish_unsent_not_to_send_count(3);
    assert_eq!(published.count_unsent_not_to_send(), 3);
    published.publish_unsent_not_to_send_count(0);
    assert_eq!(published.count_unsent_not_to_send(), 0);
}
