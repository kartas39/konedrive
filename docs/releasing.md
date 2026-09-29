# Releasing

## How a release happens

Merge `dev` into `main`. Every push to `main` runs the release workflow
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

A failed run can be rerun. If it failed after the tag was pushed, the rerun finds the tag on the
commit and reuses its version, and it replaces the files of a release that already exists instead
of making a second one.

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

The version comes from the git tags `vX.Y.Z`: a release takes the highest tag in the repository,
whatever branch it is on, and adds one to its last number. With no tag at all, the first release is
0.1.1. `scripts/version.sh` computes it, for the workflow and for `scripts/build-rpm.sh` alike:

```
scripts/version.sh release          # the version this commit is released as
scripts/version.sh local            # a local build's version
scripts/version.sh previous 0.1.2   # the tag before v0.1.2
```

**A new base.** To move to a new minor or major version, push a tag by hand, for example
`git tag v0.2.0 <commit> && git push origin v0.2.0`. The next release is then 0.2.1. A tag on the
very commit that `main` is about to build is reused, so that commit is released as 0.2.0 itself.

**Nothing is committed back.** The workflow writes the version into its own copies of
`Cargo.toml` (`[workspace.package] version`), `Cargo.lock` and the spec (`Version:` and a
`%changelog` entry); the files in git keep `0.1.0` as a placeholder. The programs show the
build's version where they already show one: `konedrivectl --version`, the user agent sent to
Microsoft Graph, and the window's application data (`konedrive --version`), which CMake reads
from `Cargo.toml` when the window is configured.

**Local builds.** `scripts/build-rpm.sh` without `--version` takes the next version with a suffix
that sorts below it: `0.1.2~dev.20260929.fad78d9` in the spec (RPM's `~`) and
`0.1.2-dev.20260929.fad78d9` in `Cargo.toml` (a semver pre-release; Cargo refuses `~`, RPM refuses
`-`). The date is `HEAD`'s commit date in UTC, the hash `HEAD`'s. So the release 0.1.2 upgrades a
local build with a plain `dnf upgrade`, and two builds of the same commit have the same version.
Run `git fetch --tags` first: only the tags the checkout has are counted, and without them a local
build may sort below a release already installed.

## A dry run

On GitHub, **Actions → Release → Run workflow**, on any branch. `dry_run` is on by default: the run
tests, builds and keeps the RPMs as its artifacts (downloadable from the run's page for 90 days),
and tags and releases nothing. Its RPMs carry a local build's version, not the release's: the
next version with a suffix that sorts below it (`0.1.2~dev.20260929.fad78d9`, as
`scripts/version.sh local` prints it), so a machine that installs them is upgraded by the release
with a plain `dnf upgrade`. Only a release is built as `X.Y.Z`. A run by hand with `dry_run` off
releases, but only from `main`; on any other branch it stays a dry run.

Locally, the same build: `git fetch --tags && scripts/build-rpm.sh --version X.Y.Z`.
