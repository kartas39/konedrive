use super::{human_bytes, parse_duration, shell_word};

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
