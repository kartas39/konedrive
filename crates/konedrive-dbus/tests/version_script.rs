//! `scripts/version.sh`, which `scripts/build-rpm.sh` and the release workflow take the version
//! from (docs/releasing.md), run in a throwaway git repository with its own Cargo.toml.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `program` in `dir`, with no git configuration but its own.
fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap_or_else(|e| panic!("cannot run {program}: {e}"))
}

fn ok(dir: &Path, program: &str, args: &[&str]) -> String {
    let out = run(dir, program, args);
    assert!(out.status.success(), "{program} {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A repository holding the script and a Cargo.toml at `version`, with `commits` commits.
fn repository(version: &str, commits: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("scripts")).unwrap();
    std::fs::copy(repo_root().join("scripts/version.sh"), dir.path().join("scripts/version.sh")).unwrap();
    // A `version =` in another table is not the workspace's.
    let toml = format!(
        "[workspace]\nmembers = []\n\n[workspace.package]\n# the next release's\nversion = \"{version}\"\n\
         edition = \"2021\"\n\n[workspace.dependencies]\nversion = \"9.9.9\"\n"
    );
    std::fs::write(dir.path().join("Cargo.toml"), toml).unwrap();
    ok(dir.path(), "git", &["init", "-q"]);
    for n in 0..commits {
        ok(dir.path(), "git", &["commit", "-q", "--allow-empty", "-m", &format!("commit {n}")]);
    }
    dir
}

#[test]
fn release_is_the_file_and_local_counts_the_commits() {
    let repo = repository("0.4.2", 3);
    let dir = repo.path();
    assert_eq!(ok(dir, "sh", &["scripts/version.sh", "release"]), "0.4.2");
    assert_eq!(ok(dir, "sh", &["scripts/version.sh", "local"]), "0.4.2~dev.3");
    assert_eq!(ok(dir, "sh", &["scripts/version.sh", "commit"]), ok(dir, "git", &["rev-parse", "HEAD"]));

    ok(dir, "git", &["commit", "-q", "--allow-empty", "-m", "one more"]);
    assert_eq!(ok(dir, "sh", &["scripts/version.sh", "local"]), "0.4.2~dev.4");

    // Tags play no part in the version, only in the release notes' previous tag.
    ok(dir, "git", &["tag", "v0.4.1", "HEAD~2"]);
    ok(dir, "git", &["tag", "v0.3.9", "HEAD~3"]);
    assert_eq!(ok(dir, "sh", &["scripts/version.sh", "release"]), "0.4.2");
    assert_eq!(ok(dir, "sh", &["scripts/version.sh", "previous", "0.4.2"]), "v0.4.1");
}

#[test]
fn a_file_without_x_y_z_is_refused() {
    let repo = repository("0.4", 1);
    let out = run(repo.path(), "sh", &["scripts/version.sh", "release"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no X.Y.Z version"), "{out:?}");
    assert!(!run(repo.path(), "sh", &["scripts/version.sh", "local"]).status.success());
}

#[test]
fn a_release_carries_only_the_files_version() {
    let repo = repository("0.4.2", 1);
    let check = |version: &str| {
        let script = ". scripts/version.sh && konedrive_check_release_version \"$PWD\" \"$1\"";
        run(repo.path(), "sh", &["-c", script, "sh", version])
    };
    assert!(check("0.4.2").status.success());
    let refused = check("0.4.3");
    assert!(!refused.status.success(), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not the version in Cargo.toml (0.4.2)"), "{refused:?}");

    // build-rpm.sh makes that check right after its arguments, before it touches anything.
    let script = std::fs::read_to_string(repo_root().join("scripts/build-rpm.sh")).unwrap();
    let check_at = script.find("konedrive_check_release_version \"$root\" \"$version\" || exit 2").expect("the check");
    assert!(check_at < script.find("rm -rf \"$top\"").expect("the clean-up"));
}

#[test]
fn rpm_orders_dev_builds_below_their_release() {
    let order = ["0.1.1~dev.9", "0.1.1~dev.12", "0.1.1", "0.1.2~dev.1"];
    let pairs: Vec<String> = order.windows(2).map(|w| format!("rpm.vercmp(\"{}\", \"{}\")", w[0], w[1])).collect();
    let lua = format!("%{{lua: print({})}}", pairs.join(" .. \" \" .. "));
    let Ok(out) = Command::new("rpm").args(["--eval", &lua]).output() else {
        eprintln!("rpm is not installed: the ordering is not checked");
        return;
    };
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "-1 -1 -1");
}
