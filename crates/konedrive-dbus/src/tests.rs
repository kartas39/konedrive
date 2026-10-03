use super::*;

#[test]
fn an_account_path_is_one_element_below_the_manager() {
    assert_eq!(account_path("3f9a1c0e5b7d").unwrap().as_str(), "/org/konedrive/Accounts/3f9a1c0e5b7d");
    for bad in ["", "a/b", "a-b", "..", "é"] {
        assert_eq!(account_path(bad), None, "{bad:?}");
    }
}
