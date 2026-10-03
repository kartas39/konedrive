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

## AC4. `LastError` is one string with many writers, cleared by prefix matching

- **Where:** `account/mod.rs:123–127, 396–400, 515`; `account/state.rs:141–153`;
  `account/sign_in.rs:141, 177, 188, 232, 251`.
- **Fix:** `mode_note: Option<ModeNote>` beside `last_error`, joined where the D-Bus property is
  read. **Size:** S to M. **Risk:** clients read the text (F64). Part of `X1`.

## AC6. Locks ordered by convention; one held across a wallet prompt

- **Where:** `account/mode.rs:117–167`; `account/sign_in.rs:102–109, 415–431`.
- **What:** the order traced is `session`, then the token cache, then config, cache and state; no
  inversion found, and nothing states it. `commit_sign_in` holds `session` across a KWallet
  prompt, intended. fsync'd file writes on the async path inside the token cache hold
  (`mode.rs:146–166`).
- **Fix:** write the order down. Changing it is not recommended. **Size:** S.

## AC8. Test-only and dead code; the quota copied by hand

- Used only by tests: `V1Config::load`, `save`, `Default`, `sync_root_upgrades_when_helper`
  (`config/migrate.rs:82–120`); `AccountService::single` (`account/mod.rs:261`) and
  `set_client_id` (`:522`); `MemoryWallet` (`account/secret.rs:37–91`). `recheck_mode`
  (`mod.rs:408`) is an alias. A doc comment on the wrong item (`config/mod.rs:952`).
- The quota's five fields are copied in four places (`account/mod.rs:583–591, 596–612`;
  `state.rs:91–95`; `quota.rs:79–101`). **Fix:** one `QuotaFigures` struct.

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
