# Code quality: `konedrivectl` and `konedrive-dbus`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `konedrivectl/src/lib.rs`, `commands/sync.rs`, `text/refusals.rs` 2; `daemon.rs`,
`commands/account.rs`, `commands/login.rs`, `text/status.rs`, `text/files.rs`, `text/uploads.rs`,
`konedrive-dbus/src/lib.rs`, `konedrive-dbus/src/testing.rs` 3; `choice.rs`,
`text/transfers.rs`, `konedrive-dbus/src/version.rs` 5; the rest 4.

**First here:** `CL2` with `CL3` as far as the enum, and `CL5`; then `CL6`.

A correction to an earlier measurement: there is no 350-line `commit` in
`konedrive-dbus/src/accounts.rs`. It is a one-line property declaration; the file is declarative.

## CL2. `refusal_text_as` gives the wrong sentence for pairs nobody wrote — **defect?**

- **Where:** `konedrivectl/src/text/refusals.rs:246–485`; wildcard arms at `:277`, `:366`,
  `:406`; a comment at `:359–360` records that this already happened once.
- **Fix:** parse the name once into a `Refusal` enum; dispatch by action group, each an
  exhaustive match; every unwritten pair falls to a generic sentence. Fold the four copies of the
  detail extraction (`:125–129, 171–175, 494–498, 556–560`).
- **Size:** M. **Risk:** low; wording is pinned by tests.
- **Verified 2026-10-03: refuted as written; a neighbouring defect confirmed by a test.**
  - **The three wildcard arms give no wrong sentence today.** Every place the daemon raises
    `NotSignedIn`, `NoHelper` and `Unsupported` was traced: each pair lands on an arm written for
    it, or on the wildcard for a registration, where its sentence is right. The arms stay a
    hazard for the next refusal someone adds.
  - **What is wrong today:** the arm at `text/refusals.rs:403–405` maps `NoRoot` for
    `Outbox | Pause | Resume | Ignore | NotUploaded | Deletes` to "the folder's sync has not
    started yet; try again in a moment". The daemon answers `NoRoot` both when no folder is
    registered at all and when the store is not opened yet
    (`konedrived/src/sync/outbox_api.rs:41–44`). Test
    `binary_an_outbox_command_with_no_folder_says_no_folder_is_registered`
    (`konedrivectl/tests/sync_cli/refusals.rs`, branch `verify-account-ctl`, ignored).
  - **Effect:** an account with no folder yet is told to wait instead of to register, every time
    one of these commands runs before `sync register`.
  - **A fix must know:** one name carries two causes. The CLI can tell them apart by
    `Folder.Path` being empty, which `explained()` already reads (`commands/sync.rs:419`); a
    second name would change the bus contract for the window too.
- **Fixed 2026-10-03** in `a62d487` (#134), the neighbouring defect: an outbox command says "no
  sync folder is registered" only when `Folder.Path` was read and is empty. For that the daemon
  publishes the path of a folder without interception before it is brought up; what that
  start-up window shows is in `docs/limitations/F199.md`.

## CL3. Refusal data travels inside English sentences

- **Where:** `text/files.rs:103–109, 123`; `text/refusals.rs:407, 467`; `text/uploads.rs:61–73`.
- **Fix:** the `Refusal` enum of `X1` in `konedrive-dbus`; later, the refused path and the
  pinning folder as structured data. **Size:** S for the enum, L for the wire.

## CL5. The label rule is stated in five places; four contradict the code — **defect?**

- **Where:** the code allows `@` (`konedrived/src/config/mod.rs:445–446`). Still saying "no @":
  `konedrivectl/src/cli.rs:80`, `text/refusals.rs:585–586`, `choice.rs:17`,
  `dbus/org.konedrive.Accounts.xml:17`; `app/accountsmodel.h:150` relies on the old rule. Also
  stale: `cli.rs:13–16`.
- **Fix:** one sentence, owned by the daemon's refusal message. **Size:** S.
- **Verified 2026-10-03: confirmed, by a test.**
  `what_the_cli_says_of_a_label_is_what_the_daemon_takes` (`konedrivectl/tests/accounts_cli.rs`,
  branch `verify-account-ctl`, ignored): `account add ann@outlook.com` succeeds, and the help of
  `account add` and the text after a refused label still say "no @".
  - **Reach the user:** `cli.rs:80` and `text/refusals.rs:585–586` (appended to every
    `InvalidArgs` of `Add` or `Rename`, whatever the reason). `choice.rs:17` is a comment;
    `dbus/org.konedrive.Accounts.xml:17` is the contract the window reads.
  - **A sixth place:** the existing test `accounts_are_added_chosen_renamed_and_removed`
    (`accounts_cli.rs:94–95`) still asserts that `account rename home a@b` is refused, and fails
    on `dev` for that reason. It is one of the eight failures #101 puts down to the client id.
  - **A fix must:** take the sentence from one place; it also omits two rules the daemon
    enforces (not 12 hexadecimal digits, no control characters).
  - **Correction:** `app/accountsmodel.h:150` does not rely on the old rule. It relies on nobody
    naming an account "Signing in…": `AccountsModel::probe` (`accountsmodel.cpp:183–193`) removes
    any such account at the next start, and both rules allow that label (limitation A15).
- **Fixed 2026-10-03** in `a62d487` (#134): the rule is `konedrive_dbus::LABEL_RULE`, used by the
  help and the refusal text; the other places are corrected. Nothing ties the sentence to
  `check_label` but a doc comment, so the two can drift again. The eight tests of #101 pass.

## CL6. `folder_command` and `sync` split one enum across two functions

- **Where:** `commands/sync.rs:76–157, 161–395`; `unreachable!` at `:385–392`; five path commands
  repeat one sequence (`:80–138`); sentences inline and repeated (`:59, 180, 191, 289, 351, 358`).
- **Fix:** split `SyncCmd` into `PathCmd` and `FolderCmd`; one `path_command` helper; sentences
  into `text/`. **Size:** M. **Risk:** low.

## CL8. The library's boundary is accidental; "text" functions that do I/O

- `lib.rs:15–16` and `text/mod.rs:9–15` glob-re-export about 70 names; `lib.rs` holds a secret
  file writer, two polling loops and `is_gone`. `text/status.rs:16–46, 79–163`: about 30 awaited
  property reads inside a text function; `text/transfers.rs:43–68` shows the right shape.
  `daemon.rs`: `shown()` returns `(Vec, bool, bool)`.
- **Fix:** named modules; read into a struct, then render purely. **Size:** S to M.

## DB1. `konedrive-dbus`

- `lib.rs` exports `pub mod testing` unconditionally (`:19`) and holds user-facing prose
  (`helper_advice`); row types in `accounts.rs` are anonymous tuples up to eight wide.

## DB2. The tests' private bus can start the installed daemon — **defect?**

- **Where:** `konedrive-dbus/src/testing.rs:12–23` (`TestBus`: `dbus-daemon --session`).
- **What:** a session `dbus-daemon` reads the standard service directories. With the package
  installed (`/usr/share/dbus-1/services/org.konedrive.Daemon.service`), a test call to
  `org.konedrive.Daemon` while the name has no owner would make the private bus start
  `/usr/bin/konedrived` with the environment of the test run: the real `~/.config` unless the run
  set another `HOME`. No test does that today (the "not running" test of
  `konedrivectl/tests/version_cli.rs` asks `NameHasOwner`).
- **Fix:** give `TestBus` a `--config-file` with no service directories. **Size:** S.
- **Found 2026-10-04, by reading; not run.** **Status: open.**
- **Fixed 2026-10-04** in `8fb6ac2` (#146): `TestBus` gives `dbus-daemon` a configuration of its
  own with no service directories; the test `the_bus_can_start_no_program` holds it.
  `tests/kio/run.sh` still starts its bus with the stock configuration
  (`docs/limitations/D32.md`).
