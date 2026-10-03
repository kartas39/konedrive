# Code quality: `konedrive-graph`

Part of the findings of the review of 2026-10-03; see [`README.md`](README.md) for how to read
them, what is unconfirmed, and the order of work. Line numbers are of `dev` at `4aeefb9`.

Scores: `lib.rs`, `graph.rs`, `drive/mod.rs`, `drive/upload.rs` 3; `pkce.rs`, `quickxor.rs` 5;
the rest 4.

**First in this crate:** `GR1` with `GR2`.

## GR1. Graph errors are strings, and throttling behaves three ways

- **Where:** `drive/mod.rs:30–49, 264–274, 309–341, 372–381, 408–417, 423–478`;
  `drive/write.rs:77–86, 171–190`; `drive/upload.rs:224–256`; `graph.rs:25–31`.
- **What:** `DriveError::Transient(String)` and `Failed(String)` carry no status, and
  `konedrived/src/remote/listing.rs:361` treats any `Failed(_)` as "Graph refused the link",
  including a 400, a 403 and an unparsable URL. Reads sleep inside `send` (up to 5 × 300 s);
  writes return `Throttled`; `upload_chunk` sleeps and probes. `send`, `send_anonymous` and
  `send_write` are one loop three times. `Thumbnail::Refused(reqwest::StatusCode)` puts a
  `reqwest` type in the public API (`mod.rs:90`).
- **Fix:** one `send` taking the auth mode and a throttle policy; one `classify(status, code)`;
  errors that carry the status and the code. **Size:** M. **Risk:** medium; retry timing changes.

## GR2. `GraphClient` duplicates `DriveClient`

- **Where:** `graph.rs:51–97`; `drive/mod.rs:146–171, 215–224`.
- **Fix:** `DriveClient::drive()` and `profile()`; delete `graph.rs`. **Size:** S.

## GR5. `TokenManager`: a network refresh under the cache lock — **defect?**

- **Where:** `token.rs:176–196`.
- **What:** for a read-write account `read_only_token` never finds a cached token and never
  stores the one it gets: every call is a token-endpoint request made while holding `cached`,
  which every Graph call of the account waits on. How often the daemon calls it was not checked.
- **Fix:** a second cache slot. **Size:** S.
- **Verified 2026-10-03: confirmed as described, by two tests; the effect is negligible.**
  `a_read_write_accounts_read_only_token_is_cached` and
  `the_accounts_own_token_does_not_wait_for_a_read_only_refresh`
  (`konedrive-graph/src/token/tests.rs`, branch `verify-tree-graph`, ignored; the second depends
  on timing, with wide margins).
  - **How often it is called:** only from `TokenExport.ReadOnly` (`dbus/token_export.rs:19`
    through `account/mode.rs:240`), which exists only in a `dev-tools` build and is called by
    `konedrivectl dev export-access-token`, by hand. Nothing periodic calls it. W11 already
    records what a process on the session bus can do with that interface.
  - **A fix must:** clear a second cache slot in `invalidate`, `forget`, `commit_as` and the
    `invalid_grant` path; never hand it out from `access_token`; keep the "refused if it can
    write" check; keep the refresh-token rotation (`token.rs:213–221`) under the lock.
- **Fixed 2026-10-03** in `b5cffd9` (#142): the cache has a slot for the read-only token; refreshes are
  one at a time under a refresh lock, and a fresh cached token is handed out during one. The
  windows this opens are in `docs/limitations/F221.md`.

## GR6. Smaller

- `MemoryStore` ("for tests", `secret.rs:34`), `TransferPool::starting_at` (`pool.rs:239`),
  `try_acquire`, `large_held` in the public API; `create_upload_session`'s unused `_size`
  (`drive/upload.rs:135`); `upload_chunk` (`:196–257`) with seven exits; `SESSION_EXPIRED` and
  `WALLET_LOCKED` are user-facing sentences in this crate; `lib.rs` has nine `pub mod` lines and
  no curated surface.
