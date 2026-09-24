# Contributing

Thanks for looking at KOneDrive. This is a young, alpha-stage project (read-only phase — see
the README), so expect things to move. A few practical notes before you send a change.

## Building

See the README's "Build dependencies (Fedora)" and "Install for your user" sections, and
[`docs/design/`](docs/design/README.md) for how the system works and why. In short:

```
sudo dnf install rust cargo cmake extra-cmake-modules gcc-c++ qt6-qtbase-devel \
  qt6-qtdeclarative-devel kf6-kirigami-devel kf6-kirigami-addons-devel kf6-ki18n-devel \
  kf6-kcoreaddons-devel kf6-kconfig-devel kf6-knotifications-devel \
  kf6-kstatusnotifieritem-devel kf6-kdbusaddons-devel kf6-kio-devel kf6-kwindowsystem-devel \
  kf6-kjobwidgets-devel dbus-daemon desktop-file-utils
cargo build --workspace
```

## Running the tests

There are three layers; run all of them before sending a change that touches the code they
cover.

**Rust workspace** (daemon, CLI, helper, the fs and proto crates):

```
cargo test --workspace
```

**Qt/Kirigami app and Dolphin plugins** (CMake + ctest):

```
cmake -S app -B build/app -DBUILD_TESTING=ON && cmake --build build/app && ctest --test-dir build/app --output-on-failure
cmake -S dolphin -B build/dolphin -DBUILD_TESTING=ON && cmake --build build/dolphin && ctest --test-dir build/dolphin --output-on-failure
```

**The VM suite** — the fanotify helper needs a real kernel and root, so its end-to-end tests run
inside a `virtme-ng` VM, never on the host:

```
tests/vm/run.sh quick   # the routine run: the end-to-end suite on btrfs (one VM)
tests/vm/run.sh full    # btrfs, ext4 and xfs, three VMs at once: slower; for changes that may
                         # behave differently per filesystem
```

`scripts/install-helper.sh` has its own test, which neither of those runs. It runs the installer as
root against a scratch tree and a fake `systemctl`, never the real system. Run it whenever you
change the installer or the helper's unit:

```
tests/vm/run.sh tests/vm/install_helper_test.sh
```

Root is never used outside that VM. If a change needs anything privileged to exercise or debug
(mounting a filesystem, running as root to poke at fanotify directly), do it inside
`virtme-ng`, not on your own machine — see `tests/vm/run.sh` for how the suite boots one.

## The limitations log

`docs/limitations-and-workarounds.md` is the one place for everything in KOneDrive that is
limited, worked around, fragile, or knowingly below the quality the project wants. If your
change accepts a limitation, builds a workaround, picks a number without measuring it, or
leaves something fragile on purpose, add an entry — the log explains its own format (kind,
evidence, status) at the top. A change is not really finished until the corner it cut is
written down there.

## Commit style

Look at `git log` before writing a commit message: this project writes commits as
`type(scope): a plain-language sentence describing what changed`, in the imperative-adjacent,
descriptive style already in the history (`fix(app): …`, `feat(daemon): …`, `docs(log): …`,
`test(vm): …`), rather than a terse label. Keep the scope to the part of the tree that changed
(`app`, `daemon`, `helper`, `dbus`, `docs`, `vm`, …).

## Licensing

By contributing, you agree your changes are licensed under GPL-3.0-or-later, the same license
as the rest of the project (see [LICENSE](LICENSE)).
