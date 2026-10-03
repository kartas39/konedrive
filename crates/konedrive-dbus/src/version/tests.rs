use super::*;

#[test]
fn a_line_names_the_program_the_version_and_the_short_commit() {
    assert_eq!(
        line("konedrived", "0.1.1-dev.57", "52545951234567890abcdef1234567890abcdef1"),
        "konedrived 0.1.1-dev.57 (commit 5254595)"
    );
    assert_eq!(line("konedrivectl", "0.1.1", "unknown"), "konedrivectl 0.1.1 (commit unknown)");
}

#[test]
fn a_plain_build_never_looks_like_a_release() {
    // `cargo test` sets no KONEDRIVE_BUILD_VERSION: the file's version with -dev.
    if option_env!("KONEDRIVE_BUILD_VERSION").is_none() {
        assert_eq!(VERSION, format!("{}-dev", env!("CARGO_PKG_VERSION")));
    }
    assert!(COMMIT == "unknown" || COMMIT.len() >= 40, "{COMMIT}");
}
