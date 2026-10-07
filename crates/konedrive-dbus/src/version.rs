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
mod tests;
