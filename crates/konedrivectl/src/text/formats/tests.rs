use super::{checked_text, grouped, human_bytes, parse_duration, seconds_text, shell_word};

#[test]
fn a_label_is_quoted_for_the_shell_only_when_it_has_to_be() {
    assert_eq!(shell_word("Family"), "Family");
    assert_eq!(shell_word("Семья"), "Семья");
    assert_eq!(shell_word("Ann's work"), r"'Ann'\''s work'");
}

#[test]
fn durations_read_as_sync_pause_takes_them() {
    assert_eq!(parse_duration("90"), Some(90));
    assert_eq!(parse_duration("30m"), Some(1800));
    assert_eq!(parse_duration("2h"), Some(7200));
    assert_eq!(parse_duration("1d"), Some(86_400));
    assert_eq!(parse_duration("1h30m"), Some(5400));
    for bad in ["", "0", "soon", "2x", "h", "30m5", "999999999999"] {
        assert_eq!(parse_duration(bad), None, "{bad:?}");
    }
}

#[test]
fn human_bytes_uses_binary_units() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(1023), "1023 B");
    assert_eq!(human_bytes(1536), "1.5 KiB");
    assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
}

/// When a folder was last checked with OneDrive: one that was never checked says so, rather
/// than a time.
#[test]
fn a_folder_never_checked_reads_never() {
    assert_eq!(checked_text(0, 1_000), "never");
    assert_eq!(checked_text(980, 1_000), "20 s ago");
    assert_eq!(checked_text(1_000 - 5 * 60, 1_000), "5 min ago");
    assert_eq!(checked_text(100_000 - 3 * 3600, 100_000), "3 h ago");
    assert_eq!(checked_text(1_000_000 - 2 * 86_400, 1_000_000), "2 d ago");
    assert_eq!(checked_text(1_010, 1_000), "just now", "a clock that went back");
}

#[test]
fn counts_set_their_thousands_apart_and_seconds_read_as_the_largest_units() {
    assert_eq!(grouped(999), "999");
    assert_eq!(grouped(1_000_000), "1 000 000");
    assert_eq!(seconds_text(3_900), "1 h 5 min");
}
