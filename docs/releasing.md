# Releasing

## How a release happens

Merge `dev` into `main`. Every push to `main` runs the release workflow
(`.github/workflows/release.yml`) on GitHub Actions, in a `fedora:44` container:

1. It installs the spec's build dependencies and runs the daemon's unit tests
   (`cargo test -p konedrived --lib`). A failing test stops the run; nothing is tagged.
2. It chooses the version (below) and builds the RPMs with `scripts/build-rpm.sh --version X.Y.Z`,
   the same script a local build uses: `konedrive` and `konedrive-kde` for Fedora 44 x86_64, and
   the source RPM.
3. It keeps them, with a `SHA256SUMS` file, as the run's artifacts.
4. It tags the commit it built `vX.Y.Z` and publishes a GitHub Release, "KOneDrive X.Y.Z", with the
   three RPMs and `SHA256SUMS`. The release notes list the pull requests merged since the previous
   tag.

It uses only the workflow's own token, with `contents: write` for the last step. Runs queue up one
after another, and a running release is never cancelled.

A failed run can be rerun. If it failed after the tag was pushed, the rerun finds the tag on the
commit and reuses its version, and it replaces the files of a release that already exists instead
of making a second one.

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
and tags and releases nothing. A run by hand with `dry_run` off releases, but only from `main`; on
any other branch it stays a dry run.

Locally, the same build: `git fetch --tags && scripts/build-rpm.sh --version X.Y.Z`.
