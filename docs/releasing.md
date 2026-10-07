# Releasing

## How a release happens

By hand, two steps:

1. Merge `dev` into `main`, with a pull request and a merge commit (not a squash: `main` must hold
   `dev`'s own commits, or the next merge finds them all again). The pull request runs the suite
   that needs root (below). The workflow below then releases `X.Y.Z`, the version in `Cargo.toml`,
   and tags the commit `vX.Y.Z`.
2. At once, a one-line pull request into `dev` that moves `version` in `[workspace.package]` of the
   root `Cargo.toml` from `X.Y.Z` to `X.Y.(Z+1)` (or to the next minor or major version, when that
   is what comes next). No other pull request changes it (below).

Every push to `main` runs the release workflow
(`.github/workflows/release.yml`) on GitHub Actions. It builds and publishes; it runs no test of
the code:

1. For Fedora 44 and for Fedora 45, each in its own container (`fedora:44`, `fedora:45`), it
   installs the spec's build dependencies, chooses the version (below) and builds the RPMs with
   `scripts/build-rpm.sh --version X.Y.Z`, the same script a local build uses, with that Fedora's
   own Rust: `konedrive` and `konedrive-kde` for x86_64, and the source RPM. The dist tag in the
   file names tells the two apart (`.fc44`, `.fc45`).
2. It keeps each Fedora's RPMs as the run's artifacts.
3. It installs each Fedora's two packages in a clean container of that Fedora, as a person would,
   and checks that every program finds its libraries, that `konedrived --version` and
   `konedrivectl --version` run, and that the window loads its interface and comes up hidden, with
   no display, on a bus of its own. A missing dependency of the package shows here.
4. It tags the commit it built `vX.Y.Z` and publishes a GitHub Release, "KOneDrive X.Y.Z", with the
   six RPMs and one `SHA256SUMS` for them all. The release notes are `docs/release-notes/X.Y.Z.md`, written by hand
   and merged with the rest before the release; without that file they list the pull requests
   merged since the previous tag.

If a build or an install check fails for either Fedora, nothing is tagged or released.

It uses only the workflow's own token, with `contents: write` for the last step. Runs go one at a
time, and a running release is never cancelled. GitHub keeps only one run waiting behind it,
though: a merge that arrives while another run already waits replaces that run, so the commit of
the replaced run gets no release of its own — the next release holds it.

A failed run can be rerun. It builds the same version (the file's), finds the tag it pushed on
the same commit and reuses it, and replaces the files of a release that already exists instead of
making a second one. A tag `vX.Y.Z` that already exists on another commit stops the run: that is
what happens when step 2 above was forgotten (the next merge into `main` still carries the version
already released) — merge the bump into `dev` and `dev` into `main` again.

Run by hand (`gh workflow run release.yml --ref <branch>`), the workflow is a dry run: it builds
and tries the packages, and tags and publishes nothing.

## Where the tests run

Not in the release: on the pull requests (`.github/workflows/tests.yml`).

**A pull request into `dev`** runs the fast tests:

- the unit tests of the daemon, the Graph client, the reasons and the tree store
  (`cargo test -p konedrived -p konedrive-graph -p konedrive-reason -p konedrive-tree --lib --features konedrived/dev-tools`),
  directly on GitHub's runner — a virtual machine with its own kernel — as its unprivileged user.
  Not in a container: Docker's default seccomp profile refuses `fanotify_init`, which the tests of
  the notification watcher need. `sudo` only sets the runner up (`dbus-daemon` for the private
  buses, and Ubuntu's AppArmor restriction on unprivileged user namespaces lifted for the one test
  that mounts a tmpfs in one);
- the window's and the Dolphin plugins' tests (`ctest`), in a `fedora:45` container, where Qt and
  KDE Frameworks are the ones the packages are built with;
- the catalogue of sentences and the generated files, with the structure rules
  (`.github/workflows/structure.yml`).

**A pull request into `main`** runs the suite that needs root — the helper, fanotify, a
filesystem of its own — on btrfs, ext4 and xfs, one machine each, all three at once:
`tests/vm/run.sh host <fs>`. A GitHub runner is itself a throwaway virtual machine with `sudo` and
a kernel new enough (7.0 on `ubuntu-26.04`), so the suite runs as root on it with no VM inside, in
a mount namespace of its own. That mode refuses to run anywhere else: on a developer's machine the
suite runs only inside the `virtme-ng` VM (`quick`, `full`). The same job runs by hand:
`gh workflow run tests.yml --ref <branch>`.

**The Rust of the tests is fixed** (`RUST_VERSION` in `tests.yml` and `structure.yml`), so that a
new compiler does not fail a pull request by itself; it is moved by hand, in a pull request. The
packages are not tied to it: each is built with its Fedora's own `rust`, as a package is.

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
and the window (`konedrive --version`, from `KAboutData`, and the foot of its sidebar, with a
second line while the running daemon is another build: `docs/design/desktop.md` §4). The rule is one for Rust
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
