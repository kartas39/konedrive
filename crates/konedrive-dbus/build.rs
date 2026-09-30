//! The version and the commit this build shows (`konedrive_dbus::build`), by the rule the window's
//! CMake build follows too (`app/CMakeLists.txt`, docs/releasing.md):
//!
//! - the version: `KONEDRIVE_BUILD_VERSION` when `scripts/build-rpm.sh` sets it (`X.Y.Z` or
//!   `X.Y.Z-dev.N`), else Cargo.toml's with `-dev`, so that a plain `cargo build` never looks like
//!   a release;
//! - the commit: `KONEDRIVE_COMMIT` when set, else `git rev-parse HEAD`, else `unknown`.

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=KONEDRIVE_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=KONEDRIVE_COMMIT");

    let version = env("KONEDRIVE_BUILD_VERSION")
        .unwrap_or_else(|| format!("{}-dev", env("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")));

    let commit = env("KONEDRIVE_COMMIT").unwrap_or_else(|| {
        let Some(head) = git(&["rev-parse", "HEAD"]).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit())) else {
            return "unknown".to_owned();
        };
        // A new commit builds again: HEAD moves, or the branch it names does (a loose ref, or
        // packed-refs once git packs it). The worktree's reflog, `logs/HEAD`, catches every
        // commit, checkout and reset even when the branch's ref is packed and packed-refs is not
        // rewritten.
        let branch = git(&["symbolic-ref", "-q", "HEAD"]);
        for reference in ["HEAD", "logs/HEAD", "packed-refs"].into_iter().chain(branch.as_deref()) {
            if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", reference]) {
                if PathBuf::from(&path).exists() {
                    println!("cargo:rerun-if-changed={path}");
                }
            }
        }
        head
    });

    println!("cargo:rustc-env=KONEDRIVE_SHOWN_VERSION={version}");
    println!("cargo:rustc-env=KONEDRIVE_SHOWN_COMMIT={commit}");
}
