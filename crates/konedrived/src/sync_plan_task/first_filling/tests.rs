use super::*;

#[test]
fn a_store_no_listing_ended_in_begins_the_first_filling() {
    let filling = FirstFilling::new();
    assert!(!filling.is_under_way());
    assert_eq!(filling.note(FirstFillingFact::RunBegan { has_listing_ended_before: false }), Some(ActivityKind::FirstSyncBegan));
    assert!(filling.is_under_way());

    let filled = FirstFilling::new();
    assert_eq!(filled.note(FirstFillingFact::RunBegan { has_listing_ended_before: true }), None);
    assert!(!filled.is_under_way());
}

#[test]
fn the_filling_ends_with_the_first_plan_after_the_listing_that_leaves_nothing_to_show() {
    let filling = FirstFilling::new();
    filling.note(FirstFillingFact::RunBegan { has_listing_ended_before: false });
    // A plan between two pages that has nothing to show ends nothing: the listing goes on.
    assert_eq!(filling.note(FirstFillingFact::PlanMade { has_folder_steps: false }), None);
    assert_eq!(filling.note(FirstFillingFact::ListingEnded), None);
    assert_eq!(filling.note(FirstFillingFact::PlanMade { has_folder_steps: true }), None);
    assert!(filling.is_under_way());
    assert_eq!(filling.note(FirstFillingFact::PlanMade { has_folder_steps: false }), Some(ActivityKind::FirstSyncEnded));
    assert!(!filling.is_under_way());
}

#[test]
fn a_filling_that_ended_stays_ended() {
    for fact in [FirstFillingFact::ListingEnded, FirstFillingFact::PlanMade { has_folder_steps: false }, FirstFillingFact::PlanMade { has_folder_steps: true }] {
        assert_eq!(decide_first_filling(FirstFillingProgress::Ended, fact), (FirstFillingProgress::Ended, None));
    }
}
