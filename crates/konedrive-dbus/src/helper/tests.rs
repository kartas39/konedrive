use super::HelperState;

#[test]
fn a_state_is_read_back_from_its_spelling_and_only_connected_needs_no_advice() {
    for state in HelperState::ALL {
        assert_eq!(HelperState::parse(state.as_str()), Some(state));
        assert_eq!(state.advice().is_none(), state == HelperState::Connected, "{state:?}");
    }
    assert_eq!(HelperState::parse("starting"), None, "a spelling this build does not know");
    assert!(HelperState::Stopped.advice().unwrap().contains("sudo systemctl start konedrive-helper"));
}
