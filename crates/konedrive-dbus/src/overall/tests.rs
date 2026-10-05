use std::collections::BTreeSet;

use super::*;

#[test]
fn a_state_and_a_reason_are_read_back_from_their_spellings() {
    for state in State::ALL {
        assert_eq!(State::parse(state.as_str()), Some(state));
    }
    for reason in Reason::ALL {
        assert_eq!(Reason::parse(reason.as_str()), Some(reason));
        assert_eq!(Overall::from(reason), Overall { state: reason.state().as_str().into(), reason: reason.as_str().into() });
    }
    let spelled: BTreeSet<_> = Reason::ALL.iter().map(|reason| reason.as_str()).collect();
    assert_eq!(spelled.len(), Reason::ALL.len(), "no spelling stands for two reasons");
    assert_eq!(Reason::parse("on-fire"), None, "a spelling this build does not know");
    assert_eq!(State::parse("on-fire"), None);
}

/// `Overall` is `(ss)` on the bus, as `dbus/org.konedrive.Folder.xml` gives it.
#[test]
fn overall_is_two_strings_on_the_bus() {
    assert_eq!(<Overall as zbus::zvariant::Type>::SIGNATURE.to_string(), "(ss)");
}
