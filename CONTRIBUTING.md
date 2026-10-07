# Contributing

Thanks for looking at KOneDrive. This is a young, alpha-stage project (read-only phase — see
the README), so expect things to move. A few practical notes before you send a change.

## Building

See [`docs/developing.md`](docs/developing.md) for the build dependencies, the development install
and the tests in full, and
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

Whenever you change the helper's unit or the syscalls it makes, run `tests/vm/run.sh unit`: it
boots the VM with systemd and starts the real unit.

Root is never used outside that VM. If a change needs anything privileged to exercise or debug
(mounting a filesystem, running as root to poke at fanotify directly), do it inside
`virtme-ng`, not on your own machine — see `tests/vm/run.sh` for how the suite boots one.

## The structure of the code

[`docs/code-map.md`](docs/code-map.md) says where everything is: every crate, directory and
file, where its tests are, how each suite is run. The structure is extended, never regrouped: a
new file goes into the crate or directory whose area it belongs to, and gets its line in the
code map in the same change.

These rules hold for every change:

1. **Size**, as advice and not as a limit: a source file of at most 1,000 lines, a test file
   of at most 1,500, a Markdown document of at most 1,200. A longer file is listed by the
   guard and is no reason to refuse a change; it is split when it holds two subjects.
2. **Tests are never in a source file.** The unit tests of `x.rs` are in `x/tests.rs`, or in
   `x/tests/` by topic. Integration tests stay in `crates/*/tests/`.
3. **Layers.** In `konedrived`, a directory of `crates/konedrived/src` uses only the
   directories before it in this order: `config/`, `account/`, `helper/`, `folder/`,
   `conditions/`, `status/`, `hydration/`, `local/`, `upload/`, `remote/`, `desktop/`, `sync/`,
   `daemon/`, `dbus/`. And `remote/` does not use `upload/`. Test code is exempt. What a lower
   directory needs from a higher one, it asks through a trait the higher one implements.
4. **One D-Bus interface, one file** in `crates/konedrived/src/dbus/`, named like its XML in
   `dbus/`.
5. **Every `impl SyncService` is in `sync/`.** The directories before `sync/` do not know
   `SyncService`.
6. **A guard**, `scripts/check-structure.sh`, checks rules 1–3 and 7. It runs on every pull request
   (`.github/workflows/structure.yml`); run it yourself before sending a change:

```
scripts/check-structure.sh
```

   The guard reads lines, not Rust. What it does not see:

   - **Layers**: only a path written as `crate::<directory>` counts (and `super::<directory>`
     in a directory's own `mod.rs`). A higher layer's item reached through a re-export, a
     `use … as` name or a path a macro builds passes. Rules 4 and 5 are not checked at all.
   - **Test code** is told by name and by declaration: a `tests` directory, `tests.rs`, and a
     module declared right under `#[cfg(test)]`, with everything below it. A module behind
     another condition (`#[cfg(any(test, feature = "…"))]`) is taken for source, and a single
     `#[cfg(test)]` function or constant in a source file is held to the layer order.
   - **Tests in a source file** are found by `#[test]`, `#[tokio::test]` and a
     `#[cfg(test)] mod … {` at the start of a line; a test attribute a macro writes is not.
   - **Size**: only `.rs`, `.cpp`, `.h`, `.qml`, `.sh`, `.py` and `.md` files are counted.
   - **The code map** is kept by hand: nothing checks that every file has its line there.

7. **Locks.** A lock of `std::sync` is taken through `crate::panic::lock`, `read` or `write`,
   which go on after a holder of the lock panicked; never with `.lock().unwrap()` or a
   recovery written out. What is done under such a lock must therefore not be able to leave
   its data half-changed. `konedrive-graph` and `konedrive-tree` have a function of their own
   for it. Not asked of test code, test doubles, the helper and the files that hold those
   functions. What the guard does with this rule:

   - **Not checked**: `crates/konedrive-helper/`, which has its own `lock` and writes the
     recovery out in a few more places (code that runs as root is not changed for tidiness);
     files named `testing.rs` or under `testing/`; and the three files that hold the functions
     (`konedrived/src/panic.rs`, `konedrive-graph/src/lib.rs`,
     `konedrive-tree/src/outbox/changes.rs`), where a second lock written with `unwrap` passes.
   - **Found**: `.lock()`, `.read()` or `.write()` with nothing passed, or `Mutex::lock(…)` and
     the like, followed in the same chain (on the line, or on the next line that is not empty
     or a comment) by `unwrap…`, `expect`, `ok`, `map_err`, `is_ok` or `is_err`.
   - **Not seen**: a lock result kept in a variable and looked at later, `match` or
     `if let Ok(…) = ….lock()`, `try_lock`, a `Condvar`'s wait, a result handed to a function
     or returned with `?`, a lock taken through another name, and a chain whose next call is
     two or more code lines away.
   - **Found wrongly**: any other `.read()` or `.write()` that takes nothing and whose result
     is unwrapped (none in the code today).
   - **A crate's first lock**: `konedrivectl`, `konedrive-dbus` and `konedrive-fs` take no
     lock of `std::sync` in source today; the first one there needs a function of its own and
     a line in the guard. What is under a `tests/` directory is test code to the guard, so a
     lock there (`tests/write-account/src/guard.rs`) is not read.

Between crates the compiler keeps the order: `konedrive-graph` and `konedrive-tree` know
nothing of the daemon.

The same workflow checks that every link in a doc comment resolves:

```
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc --locked --workspace --no-deps --document-private-items
```

It sees a broken link, not a wrong one. What it does not catch:

- **A link that resolves to another item than the one meant.** The case met: a module's file
  starts with `//!` and its `mod` line in the parent carries a `///` line. rustdoc then
  resolves the links of the file's `//!` in the parent's scope, so a bare name is looked up one
  level too high. Such a link is written with its full path (`crate::panic::write`).
- **Test code**: what is under `#[cfg(test)]` is not documented, so the links in the doc
  comments of test modules and of the `testing` fixtures are not checked.
- **A link from a public item's doc to a private item** resolves only because private items
  are documented. rustdoc warns of each, and the check does not fail on a warning.
- **What rustdoc does not read**: a `//` comment, and a name in backticks that is not a link,
  can name a file, a function or a test that is gone.
- **The compiler's version**: the workflow's `RUST_VERSION` is a second copy of the one in
  `release.yml`, and the two are moved together by hand.

## A new reason or refusal

The daemon sends codes; what a code means to a person is written once, in
`crates/konedrive-text` (`docs/design/desktop.md` §2.10), and the window, the Dolphin plugin
and `konedrivectl` all take it from there. For a new code:

1. **The code**: a variant with its key in `crates/konedrive-reason/src/lib.rs` (`Reason`,
   `LocalSkip`, `WaitsFor`), or a name in `crates/konedrive-dbus/src/refusal.rs` (`Refusal`).
2. **Its words**: an entry in the catalogue — `src/reasons.rs`, `src/waits.rs` or
   `src/files.rs` of `konedrive-text` — written as the window or the plugin shows it. One
   sentence, unless the clients must name their own controls. The crate's tests fail until
   the code has an entry; an entry may say that the code has no sentence.
3. **The generated C++**: write it again and commit it with the change:

```
KONEDRIVE_UPDATE_GENERATED=1 cargo test -p konedrive-text
```

`app/generated/` and `dolphin/src/generated/` are never edited by hand. `cargo test -p
konedrive-text` fails when they are not what the catalogue gives, and runs on every pull request
and before a release is built.

## The limitations log

`docs/limitations/` holds the technical limits KOneDrive has and cannot simply remove: the
kernel, the filesystem, OneDrive or the desktop decides, or removing the limit would give up
something the project chose to keep. It is written for whoever implements.

- If your change meets such a limit, add an entry: a file named by its id (`F53.md`) and a line
  for it in the index, `docs/limitations/README.md`. An entry is a title and four short lines:
  why the limit exists, what follows from it, why it stays, where to look. No retelling of the
  code.
- What a change in the code would remove is not a limit. Open an issue for it, written as what a
  person does and what happens then, and set `priority: low` when it can wait.
- How the code works, a decision that harms nobody and a detail of a test belong in the code and
  in `docs/design/`, not in the log.

## Commit style

Look at `git log` before writing a commit message: this project writes commits as
`type(scope): a plain-language sentence describing what changed`, in the imperative-adjacent,
descriptive style already in the history (`fix(app): …`, `feat(daemon): …`, `docs(log): …`,
`test(vm): …`), rather than a terse label. Keep the scope to the part of the tree that changed
(`app`, `daemon`, `helper`, `dbus`, `docs`, `vm`, …).

## Licensing

By contributing, you agree your changes are licensed under GPL-3.0-or-later, the same license
as the rest of the project (see [LICENSE](LICENSE)).
