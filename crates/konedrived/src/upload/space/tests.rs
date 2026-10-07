use super::*;

#[test]
fn full_is_exceeded_or_less_than_a_mebibyte_free() {
    let quota = |remaining: Option<u64>, state: &str| DriveQuota { total: 10 << 30, used: 0, remaining, state: state.into() };
    assert!(no_space(&quota(Some(5 << 30), "exceeded")));
    assert!(no_space(&quota(Some(NO_SPACE - 1), "critical")));
    assert!(!no_space(&quota(Some(NO_SPACE), "critical")));
    assert!(!known(&quota(None, "")), "no remaining and no state say nothing");
}

#[test]
fn a_too_big_reason_says_what_it_needs_and_what_is_free() {
    assert_eq!(Reason::parse(&Reason::TooBig(Some((300, 20))).to_string()).sizes(), Some((300, 20)));
    assert!(waits(Some(&Reason::TooBig(Some((1, 0))))) && waits(Some(&Reason::WaitingForSpace)));
    assert!(!waits(Some(&"quota-exceeded".into())) && !waits(None));
}
