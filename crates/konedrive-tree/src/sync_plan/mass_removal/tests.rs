use super::*;

/// Today: the limits of `local/mod.rs` and the count of `local/record.rs`.
#[test]
fn more_than_five_hundred_removals_or_more_than_a_fifth_of_the_folder_from_ten_up_are_a_mass_removal() {
    assert!(!is_mass_removal(500, 1_000_000));
    assert!(is_mass_removal(501, 1_000_000));
    assert!(!is_mass_removal(9, 10), "fewer than ten are never one");
    assert!(is_mass_removal(10, 49));
    assert!(!is_mass_removal(10, 50), "a fifth, and no more");
    assert!(!is_mass_removal(0, 0));
}
