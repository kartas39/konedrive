use super::*;

const EVERY_SENDING: [Sending; 5] = [Sending::NewDirectory, Sending::NewFile, Sending::Content, Sending::Place, Sending::Removal];

/// A file of 3 bytes in a read-write folder, with nothing in its way.
fn conditions() -> SendingConditions {
    SendingConditions {
        is_ignored: false,
        name_refusal: None,
        size: Some(3),
        largest_file_size: 100,
        quiet_until: None,
        is_onedrive_full: false,
        sending_state: SendingState::ReadWrite,
        unsent_since: Some(5),
    }
}

/// What is in the way of each sending, in the order of [`EVERY_SENDING`].
fn obstacles(conditions: &SendingConditions) -> [Option<SendingObstacle>; 5] {
    EVERY_SENDING.map(|sending| find_sending_obstacle(sending, conditions))
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
fn a_read_only_folder_sends_what_was_unsent_when_it_was_switched() {
    let read_only = Some(SendingObstacle::ReadOnlyFolder);
    let switched_at = |last_unsent_since| SendingConditions { sending_state: SendingState::ReadOnly { last_unsent_since }, ..conditions() };
    assert_eq!(obstacles(&switched_at(5)), [None, None, None, None, None], "unsent at the switch");
    assert_eq!(obstacles(&switched_at(9)), [None, None, None, None, None], "unsent before the switch");
    assert_eq!(obstacles(&switched_at(4)), [read_only.clone(), read_only.clone(), read_only.clone(), read_only.clone(), read_only.clone()], "unsent since after the switch");
    let never_stamped = SendingConditions { unsent_since: None, ..switched_at(9) };
    assert_eq!(obstacles(&never_stamped), [read_only.clone(), read_only.clone(), read_only.clone(), read_only.clone(), read_only], "with no stamp it was not unsent at the switch");
}

#[test]
fn a_name_onedrive_cannot_hold_is_in_the_way_where_the_name_is_sent() {
    let refused = Some(SendingObstacle::NameRefused(Reason::NameReserved));
    assert_eq!(obstacles(&SendingConditions { name_refusal: Some(Reason::NameReserved), ..conditions() }), [refused.clone(), refused.clone(), None, refused, None]);
}

#[test]
fn the_size_a_full_onedrive_and_the_quiet_time_are_in_the_way_where_content_is_sent() {
    let too_large = Some(SendingObstacle::TooLarge);
    assert_eq!(obstacles(&SendingConditions { size: Some(101), ..conditions() }), [None, too_large.clone(), too_large, None, None]);
    assert_eq!(obstacles(&SendingConditions { size: Some(100), ..conditions() }), [None, None, None, None, None], "the largest size is taken");
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
        sending_state: SendingState::ReadOnly { last_unsent_since: 0 },
        ..conditions()
    };
    let new_file = |conditions: &SendingConditions| find_sending_obstacle(Sending::NewFile, conditions);
    assert_eq!(new_file(&all), Some(SendingObstacle::Ignored));
    let all = SendingConditions { is_ignored: false, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::ReadOnlyFolder));
    let all = SendingConditions { sending_state: SendingState::ReadWrite, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::NameRefused(Reason::NameSpaces)));
    let all = SendingConditions { name_refusal: None, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::TooLarge));
    let all = SendingConditions { size: Some(3), ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::OneDriveFull));
    let all = SendingConditions { is_onedrive_full: false, ..all };
    assert_eq!(new_file(&all), Some(SendingObstacle::QuietTime { until: 77 }));
}
