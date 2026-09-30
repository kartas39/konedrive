//! The version and the commit this build shows, the same in `konedrived` (its `--version` and the
//! `Version` and `Commit` properties of `org.konedrive.Accounts`) and in `konedrivectl --version`.
//! Worked out at build time by `build.rs`: see there, and docs/releasing.md.

/// `X.Y.Z` (a release), `X.Y.Z-dev.N` (a package built from any other commit) or `X.Y.Z-dev` (a
/// plain `cargo build`).
pub const VERSION: &str = env!("KONEDRIVE_SHOWN_VERSION");

/// The full hash of the commit built, or `unknown`.
pub const COMMIT: &str = env!("KONEDRIVE_SHOWN_COMMIT");

/// A commit as people read it: its first 7 characters.
pub fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// `<program> <version> (commit <sha7>)`: what `--version` prints.
pub fn line(program: &str, version: &str, commit: &str) -> String {
    format!("{program} {version} (commit {})", short(commit))
}

#[cfg(test)]
mod tests {
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
}
