# Releasing

## How a release happens

By hand, two steps:

1. Merge `dev` into `main`. The workflow below releases `X.Y.Z`, the version in `Cargo.toml`, and
   tags the commit `vX.Y.Z`.
2. At once, a one-line pull request into `dev` that moves `version` in `[workspace.package]` of the
   root `Cargo.toml` from `X.Y.Z` to `X.Y.(Z+1)` (or to the next minor or major version, when that
   is what comes next). No other pull request changes it (below).

Every push to `main` runs the release workflow
(`.github/workflows/release.yml`) on GitHub Actions:

1. It runs the daemon's unit tests (`cargo test -p konedrived --lib --features dev-tools`, the token export included) on the runner itself (below).
   A failing test stops the run; nothing is built or tagged.
2. In a `fedora:44` container, it installs the spec's build dependencies, checks that the
   container's Rust is the one the tests ran with (below), chooses the version (below) and builds
   the RPMs with `scripts/build-rpm.sh --version X.Y.Z`, the same script a local build uses:
   `konedrive` and `konedrive-kde` for Fedora 44 x86_64, and the source RPM.
3. It keeps them, with a `SHA256SUMS` file, as the run's artifacts.
4. It tags the commit it built `vX.Y.Z` and publishes a GitHub Release, "KOneDrive X.Y.Z", with the
   three RPMs and `SHA256SUMS`. The release notes list the pull requests merged since the previous
   tag.

It uses only the workflow's own token, with `contents: write` for the last step. Runs go one at a
time, and a running release is never cancelled. GitHub keeps only one run waiting behind it,
though: a merge that arrives while another run already waits replaces that run, so the commit of
the replaced run gets no release of its own — the next release holds it.

A failed run can be rerun. It builds the same version (the file's), finds the tag it pushed on
the same commit and reuses it, and replaces the files of a release that already exists instead of
making a second one. A tag `vX.Y.Z` that already exists on another commit stops the run: that is
what happens when step 2 above was forgotten (the next merge into `main` still carries the version
already released) — merge the bump into `dev` and `dev` into `main` again.

## Where the tests run

The tests run directly on GitHub's `ubuntu-latest` runner — a virtual machine with its own kernel —
as the runner's own unprivileged user, not in the `fedora:44` container the RPMs are built in:
Docker's default seccomp profile refuses `fanotify_init`, so in a container every test of the
notification watcher, and every flow that relies on it, fails. `sudo` sets the runner up (it
installs `dbus-daemon` for the tests' private buses and lifts Ubuntu's AppArmor restriction on
unprivileged user namespaces, which one test mounts a tmpfs in) and is not used for the tests
themselves. Only the daemon's unit tests run there: not the workspace (#21), not the tests that
need root, and not the VM suite (#44).

**The Rust version is pinned.** The tests use the Rust of Fedora 44's `rust` package, the compiler
the RPMs are built with, installed with rustup from `RUST_VERSION` at the top of the workflow. The
build job prints the container's `rustc --version` and stops if it differs from the pin, so the
compiler that was tested and the one that built the packages cannot drift apart unnoticed.

**When Fedora 44 updates `rust`**, every run stops at that check, with an error naming the new
version, until the pin moves: set `RUST_VERSION` in `.github/workflows/release.yml` to the version
the error names (what `dnf info rust` shows in a `fedora:44` container), in a pull request into
`dev` like any other change. The tests then run with the new compiler before anything is built.

## The version

**One file holds it**: `version` in `[workspace.package]` of the root `Cargo.toml`. It is the
**next** release's version, `X.Y.Z`. Nothing else holds it: the spec's `Version:` is a placeholder
that `scripts/build-rpm.sh` fills in, and the window's CMake reads the file. Only the bump right
after a release (above) changes it, so feature pull requests never touch it and can merge in any
order. `scripts/version.sh` reads it, for the workflow and for `scripts/build-rpm.sh` alike:

```
scripts/version.sh release          # X.Y.Z: the file's version
scripts/version.sh local            # X.Y.Z~dev.N: any other build
scripts/version.sh commit           # HEAD's full hash
scripts/version.sh previous 0.1.2   # the tag before v0.1.2 (the release notes)
```

**A release** is `X.Y.Z` exactly. `scripts/build-rpm.sh --version X.Y.Z` refuses any version but
the file's. A tag only records a release; it plays no part in choosing the version.

**Any other build** — a local package, a dry run — is `X.Y.Z~dev.N` in the spec (RPM's `~`) and
`X.Y.Z-dev.N` in `Cargo.toml` and on screen (a semver pre-release; Cargo refuses `~`, RPM refuses
`-`). `N` is the number of commits in `HEAD`'s history, `git rev-list --count HEAD`: local history
only, no tags, no network. Every merge into `dev` is one squashed commit, so `N` grows with each,
and RPM orders the builds as they were made:

```
0.1.1~dev.9  <  0.1.1~dev.12  <  0.1.1  <  0.1.2~dev.1
```

A newer build upgrades an older one with a plain `dnf upgrade`, and the release `X.Y.Z` upgrades
every `X.Y.Z~dev.N`. Packages are built only from `dev` (a branch's `N` can pass a later `dev`
build's: limitations log R8).

**What is shown**, as `Version 0.1.1-dev.57 · commit 5254595`: `konedrivectl --version` (its own
build, then the running daemon's, and a line asking to restart the daemon when the two differ),
`konedrived --version`, the daemon's `Version` and `Commit` properties on `org.konedrive.Accounts`,
and the window (`konedrive --version`, from `KAboutData`). The rule is one for Rust
(`crates/konedrive-dbus/build.rs`) and for CMake (`app/CMakeLists.txt`):

- the version: `KONEDRIVE_BUILD_VERSION` when `scripts/build-rpm.sh` sets it (the spec's `%build`
  exports it for Cargo and passes `-DKONEDRIVE_BUILD_VERSION=` to CMake); otherwise, a plain
  `cargo build` or CMake build, the file's version with `-dev`, so that it never looks like a
  release;
- the commit: `KONEDRIVE_COMMIT` when set (the same way), else `git rev-parse HEAD` at build time,
  else `unknown`.

**Nothing is committed back.** `scripts/build-rpm.sh` writes the build's version into its own
copies of `Cargo.toml`, `Cargo.lock` and the spec (`Version:`, the `commit` and `build_version`
globals, and a `%changelog` entry naming the commit). The user agent sent to Microsoft Graph
carries Cargo's version of that copy.

`--dev-tools` makes a local development package whose daemon serves the token export (limitations
log W11), for the developer's own machine; it is refused together with `--version`, and CI never
passes it.

## A dry run

On GitHub, **Actions → Release → Run workflow**, on any branch. `dry_run` is on by default: the run
tests, builds and keeps the RPMs as its artifacts (downloadable from the run's page for 90 days),
and tags and releases nothing. Its RPMs carry a local build's version, not the release's: the
file's version with a suffix that sorts below it (`0.1.2~dev.57`, as `scripts/version.sh local`
prints it), so a machine that installs them is upgraded by the release with a plain `dnf
upgrade`. Only a release is built as `X.Y.Z`. A run by hand with `dry_run` off
releases, but only from `main`; on any other branch it stays a dry run.

Locally, the same build: `scripts/build-rpm.sh --version X.Y.Z`, with `X.Y.Z` the file's version.
