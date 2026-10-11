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
    published_plan.publish(answers.clone(), BTreeMap::new());
    assert_eq!(published_plan.find_answer(item(5)), Some(ItemAnswer::Planned(StepClass::Content)));
    assert_eq!(published_plan.find_answer(item(6)), Some(ItemAnswer::Excluded(ExclusionReason::NameIgnored)));
    assert_eq!(published_plan.find_answer(item(7)), None, "an item the plan was not given");
    assert_eq!(published_plan.list_answers(), answers);
}

#[test]
fn a_new_plan_replaces_the_answers_of_the_one_before() {
    let published_plan = PublishedPlan::new();
    published_plan.publish(BTreeMap::from([(item(5), ItemAnswer::Planned(StepClass::Content)), (item(6), ItemAnswer::Planned(StepClass::Metadata))]), BTreeMap::new());
    let next = BTreeMap::from([(item(6), ItemAnswer::Excluded(ExclusionReason::CannotBeShown))]);
    published_plan.publish(next.clone(), BTreeMap::new());
    assert_eq!(published_plan.find_answer(item(5)), None, "the item is not in the new plan: its answer is gone");
    assert_eq!(published_plan.list_answers(), next);
}
