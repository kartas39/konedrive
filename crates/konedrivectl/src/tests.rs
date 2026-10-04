/// Issue #21: the browser opens only on a terminal, and never with the variable set.
#[test]
fn the_browser_opens_only_on_a_terminal_without_the_variable() {
    use std::ffi::OsStr;
    assert!(super::opens_browser(None, true));
    assert!(super::opens_browser(Some(OsStr::new("")), true));
    assert!(!super::opens_browser(None, false));
    assert!(!super::opens_browser(Some(OsStr::new("1")), true));
    assert!(!super::opens_browser(Some(OsStr::new("1")), false));
}
