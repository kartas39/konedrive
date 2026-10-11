


use super::*;

fn aside_name(number: i64) -> std::ffi::OsString {
    make_aside_entry_name(konedrive_tree::LocalItemId::new(number))
}

fn temporary_name(number: i64) -> std::ffi::OsString {
    konedrive_tree::make_temporary_name(konedrive_tree::LocalItemId::new(number), 1)
}

#[test]
fn an_entry_set_aside_is_an_entry_no_row_says_and_a_hidden_name() {
    assert!(is_rowless_entry_name(&aside_name(7)), "the check skips it");
    assert!(is_hidden_name(&aside_name(7)), "the mount hides it");
    assert!(aside_name(7).to_str().unwrap().starts_with(ASIDE_ENTRY_PREFIX));
}

/// The two rules give opposite answers here: skipping the name would make the check find
/// the row's file gone.
#[test]
fn a_temporary_name_is_a_hidden_name_and_not_an_entry_no_row_says() {
    for name in [temporary_name(7), konedrive_tree::make_temporary_name(konedrive_tree::LocalItemId::new(7), 2)] {
        assert!(name.to_str().unwrap().starts_with(TEMPORARY_NAME_PREFIX), "the prefix here is the tree's: {name:?}");
        assert!(is_hidden_name(&name), "{name:?}");
        assert!(!is_rowless_entry_name(&name), "{name:?}");
    }
}

#[test]
fn the_holding_directory_and_a_new_folder_are_both() {
    for name in [HOLDING.to_owned(), format!("{NEW_PREFIX}abc")] {
        assert!(daemon_owned(name.as_ref()), "{name}");
        assert!(is_rowless_entry_name(name.as_ref()) && is_hidden_name(name.as_ref()), "{name}");
    }
}

#[test]
fn the_set_aside_name_of_an_item_is_the_same_at_every_start_and_another_items_is_another() {
    assert_eq!(aside_name(7), aside_name(7));
    assert_eq!(aside_name(7), std::ffi::OsString::from(".konedrive-aside-7"));
    assert_ne!(aside_name(7), aside_name(71));
    assert_ne!(aside_name(7), aside_name(-7));
}

#[test]
fn a_name_of_the_users_is_neither() {
    for name in ["docs", ".hidden", "konedrive-aside-7", ".konedrive", ".konedrive-asid", ""] {
        assert!(!is_rowless_entry_name(name.as_ref()) && !is_hidden_name(name.as_ref()), "{name:?}");
    }
}
