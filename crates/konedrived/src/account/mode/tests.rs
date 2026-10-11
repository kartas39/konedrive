use super::*;

/// A switch to read-only is made at once with nothing unsent and whenever it is forced;
/// asked with something unsent and not forced, the wish is kept. The count is carried
/// either way: what stays here under the hold, or what is still to send.
#[test]
fn a_switch_to_read_only_is_made_now_with_nothing_unsent_or_forced_and_is_a_wish_otherwise() {
    let cases = [
        (0, false, ReadOnlySwitch::SwitchNow { unsent: 0 }),
        (0, true, ReadOnlySwitch::SwitchNow { unsent: 0 }),
        (1, true, ReadOnlySwitch::SwitchNow { unsent: 1 }),
        (u64::MAX, true, ReadOnlySwitch::SwitchNow { unsent: u64::MAX }),
        (1, false, ReadOnlySwitch::KeepWish { unsent: 1 }),
        (250, false, ReadOnlySwitch::KeepWish { unsent: 250 }),
    ];
    for (unsent, is_forced, switch) in cases {
        assert_eq!(decide_read_only_switch(unsent, is_forced), switch, "{unsent} unsent, forced {is_forced}");
    }
}
