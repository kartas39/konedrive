# Code quality: `account/` and `config/`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `account/mod.rs`, `account/sign_in.rs`, `account/mode.rs`, `account/secret.rs`,
`config/mod.rs` 3; `account/state.rs`, `account/quota.rs`, `account/cache.rs`,
`config/migrate.rs` 4.

**First in this area:** `AC1` with `AC4`; then `CF1`.

## AC1. The sign-in attempt is written twice — **defect?** in part

- **Where:** `account/sign_in.rs:17–43, 265–271, 401–465`; `account/mode.rs:51–79, 84–184`.
- **What:** `mode.rs:62–76` re-checks the state under the session lock; `begin_sign_in` sets
  `SigningIn` at `sign_in.rs:13`, awaits `LoopbackListener::bind()` at `:17`, and takes the lock
  at `:36`. **defect?** a `cancel_sign_in` in that gap sets `SignedOut` and bumps the generation;
  `begin_sign_in` bumps it again and spawns an attempt that `commit_sign_in` accepts. "Cancelled"
  is an empty string (`sign_in.rs:279, 458`; `mode.rs:180`). Every failure is a `String`.
- **Fix:** one `start_attempt(kind)` that checks, bumps the generation and swaps the cancel
  channel under the session lock; `enum AttemptEnd { Cancelled, Failed(SignInError) }`.
- **Size:** M. **Risk:** medium to high: the identity guard and the write gate's order.
- **Verified 2026-10-03: confirmed, by a test for the mechanism and a trace for the cancel.**
  `a_sign_in_that_answered_a_url_is_shown_as_signing_in` (`konedrived/tests/account_flow.rs`,
  branch `verify-account-ctl`, ignored): forced with a sign-out held in the wallet's delete, the
  account shows `SignedOut` while an attempt is live, and the browser's answer then signs it in
  and stores the refresh token. `commit_sign_in` checks only the generation and `is_retired`
  (`sign_in.rs:417`), never the state. The cancel takes the same path by trace
  (`sign_in.rs:71–83`, then `:36–44`); a test cannot force it without a hook.
  - **What starts it:** zbus runs each method call in its own task and the runtime is
    multi-threaded, so `BeginSignIn` and `CancelSignIn` or `SignOut` on one account run in
    parallel.
  - **Effect:** `konedrivectl login` prints "sign-in was cancelled" and exits non-zero; finishing
    in the browser then signs the account in, after the user was told otherwise. No data is lost;
    the identity guard still runs. Likelihood: low; the cancel's window is tens to hundreds of
    microseconds and needs a second client.
  - **A fix must:** make the state change, the generation bump and the cancel-channel swap one
    step under the session lock, or re-check the state there as `mode.rs:66` does; not leave
    `SigningIn` behind on a refused start; keep the order `session`, then the token cache.
  - **Corrections:** `LoopbackListener::bind().await` at `:17` is not a suspension point; the gap
    needs a second thread or a contended lock at `:36`. `sign_out` (`:98–120`) does the same as
    the cancel, with a far wider window.
- **Fixed 2026-10-03** in `9cbcecf` (#140): the start of an attempt is one step under the session
  lock, after an unlocked check that answers what `dev` answered at once; `commit_sign_in`
  refuses unless the account still shows `signing-in`. A refresh that reports signed-out during
  a sign-in now ends it in silence but for a warning.

## AC4. `LastError` is one string with many writers, cleared by prefix matching

- **Where:** `account/mod.rs:123–127, 396–400, 515`; `account/state.rs:141–153`;
  `account/sign_in.rs:141, 177, 188, 232, 251`.
- **Fix:** `mode_note: Option<ModeNote>` beside `last_error`, joined where the D-Bus property is
  read. **Size:** S to M. **Risk:** clients read the text (F64). Part of `X1`.
- **Fixed 2026-10-04** in `4a0919c` (#157): the mode's note is a slot of its own (`account/state.rs`, `set_error`,
  `clear_error`, `set_mode_note`), joined with the error in `published_error()`; the text of the
  property is as before.
- **Kept as it was, a candidate defect:** an account error hides the mode note. While an error
  stands, the note is not shown, and a note set over an error drops the error for good.

## AC6. Locks ordered by convention; one held across a wallet prompt

- **Where:** `account/mode.rs:117–167`; `account/sign_in.rs:102–109, 415–431`.
- **What:** the order traced is `session`, then the token cache, then config, cache and state; no
  inversion found, and nothing states it. `commit_sign_in` holds `session` across a KWallet
  prompt, intended. fsync'd file writes on the async path inside the token cache hold
  (`mode.rs:146–166`).
- **Fix:** write the order down. Changing it is not recommended. **Size:** S.
- **Fixed 2026-10-04** in `1f7e3d7` (#176): the lock order is written at the top of `account/mod.rs`; a
  reading of every path found no inversion. No test holds the order.

## AC8. Test-only and dead code; the quota copied by hand

- Used only by tests: `V1Config::load`, `save`, `Default`, `sync_root_upgrades_when_helper`
  (`config/migrate.rs:82–120`); `AccountService::single` (`account/mod.rs:261`) and
  `set_client_id` (`:522`); `MemoryWallet` (`account/secret.rs:37–91`). `recheck_mode`
  (`mod.rs:408`) is an alias. A doc comment on the wrong item (`config/mod.rs:952`).
- The quota's five fields are copied in four places (`account/mod.rs:583–591, 596–612`;
  `state.rs:91–95`; `quota.rs:79–101`). **Fix:** one `QuotaFigures` struct.
- **Fixed 2026-10-04** in `1f7e3d7` (#176): the version-1 load/save code, `set_client_id` and the
  `recheck_mode` alias are gone; `account::testing` is behind a feature (`D43`); one `QuotaFigures`.

## CF1. `config/mod.rs` is at the size limit and its API is primitive

- **Where:** `config/mod.rs`, 969 lines of the 1,000 allowed; `:511`, `:521`, `:675`, `:809–824`,
  `:874`, `:891`; `config/migrate.rs:154–169`.
- **What:** the next key breaks the guard. `ConfigStore::writes_allowed(id)` takes an account id
  and `Config::writes_allowed(drive_id)` a drive id, both `&str`, both used in `account/mode.rs`
  (`:37`, `:292`): a swap compiles. Tuples returned (`:891`). `write_standing` and `current`
  re-read and parse the file under the store mutex on every call; `sync`'s `write_gate` calls it
  before each outbox row and between upload fragments (`upload/engine.rs:412`,
  `upload/content.rs:170`).
- **Fix:** `paths.rs`, `model.rs`, `store.rs`, `atomic.rs`; `AccountId` and `DriveId` newtypes; a
  `WriteStanding` struct. **Size:** M. **Risk:** low to medium.
- **Fixed in part 2026-10-04** in `8a4a9a6` (#153), the re-read on the write path: the upload worker asks
  the write gate in one blocking section. The file is still read and parsed at every asking,
  and other readers stay on runtime threads. The split of the file,
  the newtypes and `WriteStanding` are `B12`'s.
- **Fixed 2026-10-04** in `1f7e3d7` (#176): `config/` is `paths.rs`, `ids.rs`, `model.rs`, `store.rs`,
  `atomic.rs`; `AccountId` and `DriveId` are types; `write_standing` answers one `WriteStanding`.

