use super::*;

#[test]
fn an_account_path_is_one_element_below_the_manager() {
    assert_eq!(account_path("3f9a1c0e5b7d").unwrap().as_str(), "/org/konedrive/Accounts/3f9a1c0e5b7d");
    for bad in ["", "a/b", "a-b", "..", "é"] {
        assert_eq!(account_path(bad), None, "{bad:?}");
    }
}

/// Only the bus's answer for a property that is not there says the daemon has none; an
/// object that is gone, or a failed call, does not.
#[test]
fn a_property_the_daemon_does_not_have_is_told_from_other_failures() {
    let read = |error: zbus::fdo::Error| zbus::Error::FDO(Box::new(error));
    assert!(is_unknown_property(&read(zbus::fdo::Error::UnknownProperty("Writable".to_owned()))));
    assert!(!is_unknown_property(&read(zbus::fdo::Error::UnknownObject("no such account".to_owned()))));
    assert!(!is_unknown_property(&read(zbus::fdo::Error::Failed("no".to_owned()))));
    assert!(!is_unknown_property(&zbus::Error::InvalidReply));
}
