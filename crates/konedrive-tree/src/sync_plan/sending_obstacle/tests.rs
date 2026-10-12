use konedrive_reason::Reason;

use super::*;
use crate::sync_plan::NextTry;

const EVERY_SENT_PART: [SentPart; 5] = [SentPart::NewDirectory, SentPart::NewFile, SentPart::Content, SentPart::Place, SentPart::Removal];

/// A file of 3 bytes in a read-write folder, with nothing in its way.
fn conditions() -> SendingConditions {
    SendingConditions {
        is_ignored: false,
        name_refusal: None,
        size: Some(3),
        quiet_until: None,
        is_onedrive_full: false,
        refusal: None,
        next_try: None,
        now: 500,
    }
}

/// What is in the way of each part, in the order of [`EVERY_SENT_PART`].
fn obstacles(conditions: &SendingConditions) -> [Option<SendingObstacle>; 5] {
    EVERY_SENT_PART.map(|sent_part| find_sending_obstacle(sent_part, conditions))
}

#[test]
fn with_nothing_in_its_way_everything_is_sent() {
    assert_eq!(obstacles(&conditions()), [None, None, None, None, None]);
}

#[test]
fn nothing_of_an_ignored_item_is_sent() {
    let ignored = Some(SendingObstacle::Ignored);
    assert_eq!(obstacles(&SendingConditions { is_ignored: true, ..conditions() }), [ignored.clone(), ignored.clone(), ignored.clone(), ignored.clone(), ignored]);
}

#[test]
fn a_name_onedrive_cannot_hold_is_in_the_way_where_the_name_is_sent() {
    let refused = Some(SendingObstacle::NameRefused(Reason::NameReserved));
    assert_eq!(obstacles(&SendingConditions { name_refusal: Some(Reason::NameReserved), ..conditions() }), [refused.clone(), refused.clone(), None, refused, None]);
}

#[test]
fn a_full_onedrive_and_the_quiet_time_are_in_the_way_where_content_is_sent() {
    let full = Some(SendingObstacle::OneDriveFull);
    assert_eq!(obstacles(&SendingConditions { is_onedrive_full: true, ..conditions() }), [None, full.clone(), full, None, None]);
    let quiet = Some(SendingObstacle::QuietTime { until: 77 });
    assert_eq!(obstacles(&SendingConditions { quiet_until: Some(77), ..conditions() }), [None, quiet.clone(), quiet, None, None]);
}

#[test]
fn of_several_obstacles_the_one_that_does_not_end_by_itself_is_said_first() {
    let all = SendingConditions {
        is_ignored: true,
        name_refusal: Some(Reason::NameSpaces),
        size: Some(101),
        quiet_until: Some(77),
        is_onedrive_full: true,
        ..conditions()
    };
    let new_file = |conditions: &SendingConditions| find_sending_obstacle(SentPart::NewFile, conditions);
    assert_eq!(new_file(&all), Some(SendingObstacle::Ignored));
    let all = SendingConditions { is_ignored: false, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::NameRefused(Reason::NameSpaces)));
    let all = SendingConditions { name_refusal: None, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::OneDriveFull));
    let all = SendingConditions { is_onedrive_full: false, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::QuietTime { until: 77 }));
}

/// What the steps remember: nothing of an item OneDrive refused is sent, whatever part, and
/// nothing of one whose next try is later, until that time has come.
#[test]
fn what_onedrive_refused_and_what_is_tried_again_later_is_not_sent() {
    let refused = SendingConditions { refusal: Some(Reason::NotAllowed(None)), ..conditions() };
    assert_eq!(obstacles(&refused), [(); 5].map(|()| Some(SendingObstacle::Refused(Reason::NotAllowed(None)))));
    let next_try = Some(NextTry { not_before: 560, reason: Reason::Network });
    let later = SendingConditions { next_try: next_try.clone(), ..conditions() };
    assert_eq!(obstacles(&later), [(); 5].map(|()| Some(SendingObstacle::TriedAgainLater { until: 560, reason: Reason::Network })));
    assert_eq!(obstacles(&SendingConditions { now: 560, ..later.clone() }), [None, None, None, None, None], "the time has come");
    // What waits for the user is said before what waits for a time.
    assert_eq!(obstacles(&SendingConditions { refusal: Some(Reason::TooLarge), ..later })[0], Some(SendingObstacle::Refused(Reason::TooLarge)));
}
