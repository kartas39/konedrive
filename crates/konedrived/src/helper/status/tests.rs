use super::*;

#[test]
fn systemds_states_map_to_the_helpers() {
    assert_eq!(state_of_unit("not-found", "inactive"), HelperState::NotInstalled);
    assert_eq!(state_of_unit("loaded", "inactive"), HelperState::Stopped);
    assert_eq!(state_of_unit("loaded", "deactivating"), HelperState::Stopped);
    assert_eq!(state_of_unit("loaded", "failed"), HelperState::Failed);
    assert_eq!(state_of_unit("bad-setting", "inactive"), HelperState::Failed);
    assert_eq!(state_of_unit("loaded", "active"), HelperState::Unknown, "running, and no link yet");
}
