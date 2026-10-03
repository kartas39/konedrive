# Code quality findings

What a review of the code found after the refactoring into modules (#107): where the code inside
the modules is hard to change safely, and where it may be wrong. It is a list of problems to
work through, kept here so that each is recorded once. It is not the limitations log
(`docs/limitations/`), which records what is knowingly accepted; a finding that is confirmed and
not fixed moves there.

- Reviewed: `dev` at `4aeefb9`, 2026-10-03. Line numbers are of that commit.
- How: eight read-only reviews, one for each area, each of which read every production file of
  its area in full. Test code was not read.

## How to read the findings

- **A finding is unconfirmed unless it says otherwise.** Every finding comes from reading the
  code, without building or running it. A finding marked **defect?** claims the program does
  something wrong. The candidates in the table below were each checked on 2026-10-03, by a test
  or by a traced path: the table has the verdict, and the finding has a "Verified" paragraph with
  the evidence, the effect, and what a fix has to take care of. Every other finding is still
  only read.
- **Coverage is guessed.** Where a finding says tests cover something, that rests on the size
  and name of the test file, not on its assertions.
- **The limitations log was searched, not read whole.** A finding may already be recorded in
  `docs/limitations/` under words the reviewer did not search for. Check before recording it
  again.
- Paths of the daemon are relative to `crates/konedrived/src/`. Other paths start with the
  crate's name (`konedrive-tree/src/…` means `crates/konedrive-tree/src/…`).
- **Size:** S is hours, M is a day or two, L is more. **Risk** is the risk of making the change.
- Each finding has an id (`RE1`, `TR3`, …) that does not change. A finding that is closed is
  marked, not deleted.
- **Status** of a candidate for a defect: *open* (not looked at), *confirmed* (a test or a traced
  path shows it, named in the finding), *refuted* (with the reason), *fixed* (with the commit).

## The files

| File | Area | Ids |
|---|---|---|
| [`across.md`](across.md) | Across areas | `X` |
| [`remote.md`](remote.md) | `remote/` and `status/` | `RE` |
| [`sync.md`](sync.md) | `sync/`, `daemon/`, `dbus/` | `SY` |
| [`tree.md`](tree.md) | `konedrive-tree` | `TR` |
| [`upload.md`](upload.md) | `upload/` | `UP` |
| [`local.md`](local.md) | `local/`, `conditions/`, `desktop/` | `LO` |
| [`hydration.md`](hydration.md) | `hydration/`, `folder/`, `helper/` (the daemon's side) | `HY` |
| [`helper.md`](helper.md) | `konedrive-helper`, `konedrive-fs`, `konedrive-proto` | `HE, PR, FS` |
| [`graph.md`](graph.md) | `konedrive-graph` | `GR` |
| [`account-config.md`](account-config.md) | `account/` and `config/` | `AC, CF` |
| [`ctl.md`](ctl.md) | `konedrivectl` and `konedrive-dbus` | `CL, DB` |

## Order of work

1. **Confirm or refute the candidates for defects** (the next section): a test for each, then
   the fix. They are cheap to confirm, and several sit on the paths that decide whether
   something is deleted in OneDrive or whether an application reads zeros.
2. **The two problems that cross every area:** reasons and states kept as strings (`X1`), and
   blocking file I/O on async threads (`X2`).
3. **By area, in this order:** `remote/` with the leaving mechanism, `sync/`, `konedrive-tree`,
   `upload/`, `local/`, `hydration/` with `folder/` and `helper/`, the helper, `konedrive-graph`,
   the account and the configuration, `konedrivectl`. Each area's section says what to do first
   in it.

## Candidates for defects, to confirm first

| Id | Where | What it would do | Status |
|---|---|---|---|
| [`SY1`](sync.md) | `sync/write_mode.rs:593`, `:426`; `remote/listing/rw.rs:131`, `:206` | Two lock orders; a hang that stops fills for every account | confirmed |
| [`TR1`](tree.md) | `konedrive-tree/src/reconcile.rs:216–274` | A commit said to be one transaction is two; deferred changes lost at a crash between them | confirmed |
| [`TR2`](tree.md) | `konedrive-tree/src/lib.rs:710`, `outbox/worker.rs:41`, `outbox.rs:634` | "Forget the local objects below" in three copies with different reach (delete safety) | refuted as a defect; the difference is real |
| [`UP2`](upload.md) | `upload/content.rs:41`, `:209`, `:852` | The id of a bad upload is lost; the next send makes a conflict copy against the worker's own upload | confirmed |
| [`UP1`](upload.md) | `upload/engine.rs:307–317`, `engine/drain.rs:247`, `:255` | The "needs sign-in" latch is never cleared; one 401 or 403 stops the worker | confirmed for 403; the 401 part refuted |
| [`UP3`](upload.md) | `upload/kept_back.rs:64–81` | Blocked rows shown as "waiting, goes up by itself" | confirmed |
| [`LO3`](local.md) | `local/examine/found.rs:200`, `classify.rs:340`, `:362` | One file's I/O error aborts the whole examination, again at every retry | refuted as written; a narrower form confirmed |
| [`LO4`](local.md) | `local/watcher/mod.rs:544–641` | The examiner thread dies unnoticed; uploads stop with no error shown | fixed in `eba0828` (the panic); refuted (`RootGone`) |
| [`LO13`](local.md) | `local/entry.rs:144`, `local/examine/run.rs:145` | A mount without user attributes inside the folder aborts every examination | open |
| [`HY1`](hydration.md) | `hydration/source/fill.rs:196–249`, `:351` | A fill without clearance punches a file on the strength of a comment (the zeros path) | refuted as reachable; a latent hole confirmed |
| [`HY8`](hydration.md) | `hydration/dehydrate.rs:220` | An error path skips the roll-back; a good local copy is lost | confirmed |
| [`HY7`](hydration.md) | `hydration/pin.rs:642` | A cancelled download is reported to the pool as a success | confirmed; low impact |
| [`HY5`](hydration.md) | `hydration/server.rs:257` | An errno the kernel cannot deliver; the opener gets `EIO`, not "gone" | confirmed (the errno); one sub-claim refuted |
| [`HE1`](helper.md) | `konedrive-helper/src/jobs.rs:211`, `:238` | No bound per uid on suspended opens; one user can deny every other user's opens | confirmed |
| [`HE2`](helper.md) | `konedrive-helper/src/connection.rs:285`, `roots.rs` | Roots unbounded per uid, `root_id` unvalidated, a re-registration that leaves marks | confirmed |
| [`AC1`](account-config.md) | `account/sign_in.rs:13–36` | A cancelled sign-in can complete | confirmed |
| [`RE6`](remote.md) | `remote/listing.rs:639` against `:645` | The same store failure is "blocking" or not by the line it happens on | confirmed; small |
| [`RE1`](remote.md) | `remote/materialize/rw.rs:651–669` | `copy_aside` renames a directory and does not rebase the `leaving` row's path | refuted (the `copy_aside` claim) |
| [`RE10`](remote.md) | `remote/materialize/replace.rs:249`, `listing/replacements.rs:180` | A replacement cancelled between the swap and the record leaves the new inode unrecorded | refuted (the cancelled replacement) |
| [`GR5`](graph.md) | `konedrive-graph/src/token.rs:176–196` | Every read-only token for a read-write account is a network request under the cache lock | confirmed; effect negligible |
| [`FS1`](helper.md) | `konedrive-fs/src/placeholder.rs:229–231` | An item dated before 1970 cannot get a placeholder | refuted for OneDrive items; confirmed for the function |
| [`CL2`](ctl.md) | `konedrivectl/src/text/refusals.rs:277`, `:366`, `:406` | A wrong sentence for a refusal and action nobody wrote | refuted as written; a neighbouring defect confirmed |
| [`CL5`](ctl.md) | `konedrivectl/src/cli.rs:80`, `text/refusals.rs:585` and three more | The label rule stated five times; four say "no @", the code allows it | confirmed |
| [`SY5`](sync.md) | `daemon/manager.rs:323–325`, `:302` | `Accounts.Remove` that fails half-way leaves an account that refuses everything | confirmed |
| [`SY6`](sync.md) | `sync/mod.rs:72–78` | A typo in `config.toml` silently makes a OneDrive folder local | confirmed |

### The order of the fixes

Ranked by what the user loses, how silently, and how likely; a small fix does not wait for the
structural change behind it (`X1`, `X2`), which comes after.

1. **Work that stops with nothing shown.**
   - `SY1`: the hang; it can stop fills for every account.
   - `UP1`: one `403` stops every upload of the account, and nothing says so.
   - `LO4`: the examiner dies and uploads stop; no panic is reachable today, but the guard is
     small and any later bug in the examination would land here.
   - `TR1`: the disk and the store disagree for good after a failure between two commits.
2. **A wrong result the user sees.**
   - `UP2`: a conflict copy against the worker's own upload.
   - `UP3`: blocked rows shown as going up by themselves.
   - `SY5`: an account whose removal failed half-way refuses everything.
   - `CL5` and the neighbour of `CL2`: wrong sentences, every time; with them the stale test
     that #101 misattributes.
3. **The helper's bounds:** `HE1`, `HE2`. Root code, but only a hostile local user reaches them,
   so they matter on a machine with several users.
4. **Rare or small:** `AC1`, `HY8`, `SY6`, `RE6`, `HY5`, `HY7`, the narrow form of `LO3`, `FS1`,
   `GR5`. With `HY1`'s latent hole closed here too, since it sits on the zeros path.
5. **To confirm:** `LO13`, in the VM.

Each fix is a pull request into `dev` that carries the test of its finding from the `verify-*`
branch, without the `#[ignore]`.

### The tests behind the verdicts

The tests are on branches that are not merged, one for each group of candidates. A test that
fails because of a defect carries `#[ignore = "shows <ID>: …"]`; it is merged, without the
attribute, with the fix of its finding.

| Branch | Candidates |
|---|---|
| `verify-sync-remote` | `SY1`, `SY5`, `SY6`, `RE6`, `RE1`, `RE10` |
| `verify-tree-graph` | `TR1`, `TR2`, `GR5` |
| `verify-upload` | `UP1`, `UP2`, `UP3` |
| `verify-local` | `LO3`, `LO4` |
| `verify-hydration` | `HY1`, `HY5`, `HY7`, `HY8`, `FS1` |
| `verify-helper` | `HE1`, `HE2` |
| `verify-account-ctl` | `AC1`, `CL2`, `CL5` |

### Found while verifying

- [`LO13`](local.md): a filesystem without user attributes mounted inside the folder should abort
  every examination. Traced, not run.
- One of the eight test failures that #101 puts down to the built-in client id has another
  cause: `accounts_are_added_chosen_renamed_and_removed` (`konedrivectl/tests/accounts_cli.rs`)
  still asserts that a label with "@" is refused (see `CL5`).

## What is good and is left alone

- **Helper:** `by_handle.rs`; the queue of `outbox.rs`; the credit and retire logic of `jobs.rs`;
  `classify_read_failure`; `open_beneath` and `reopen_and_verify`; the lock discipline (no two
  `Shared` locks held at once).
- **Stores and transfers:** the owner thread (`konedrive-tree/src/shared.rs`); `TransferPool`'s
  scheduling; `Source` and `step`; `WriteError`; the merge table of `outbox/record.rs`; the
  portioned pick of `outbox/pick.rs`.
- **Daemon:** `folder/locks.rs`; `remote/live.rs`; the turn and cancellation design of
  `remote/listing.rs`; the identity logic of `remote/materialize/replace.rs`;
  `local/watcher/{fan,map,dirt}.rs`; `local/ignore.rs`; `conditions/`; `desktop/baloo.rs`;
  `daemon/stop.rs`; the order in `daemon/startup.rs`; `sync/free_up.rs` `free_one`;
  `HelperHub::route`; `hydration/source/fill.rs` `commit`; the walk of `hydration/recovery.rs`;
  `ConfigStore::update` and `write_atomic`; `recompute_mode`; `AccountSecrets`.
- **Upload:** the lock discipline; persist-before-send and the commit order; `local.rs`;
  `move_out/{walk,place,trash}.rs`; the `content::Job` struct; `outcome_of`.
- **CLI:** `choice.rs`; `text/transfers.rs`; the exit codes.

## Not judged

- No test file was read. No design document was read against the code, except single sections.
- The findings between areas were not reconciled by one reader: two reviews may describe one
  problem from two sides (`RE1` and `LO9`; `UP2` and `TR4`; `HY7` and `CL3`).
- The window and the Dolphin plugins (C++) were not reviewed.
