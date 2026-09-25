# Upload stress tool

Chosen: plain Python (stdlib only), with its own Markdown report.
Why: pytest is not installed and not worth adding for one report; a JUnit/pytest-md report is
built for test-runner UIs, not "every problem plus CLI state plus daemon logs, one file to paste
to an LLM", which a report this script writes directly gives for free, with nothing to install.

## What it does

It creates, edits, moves, and deletes real files under a read-write account's synced folder, and
after every scenario checks that the outbox drained cleanly, that nothing of the run is stuck
locally, that the item counts agree after a refresh, and — **read-only, straight against
Microsoft Graph** (`graph_check.py`, `urllib` only, GET only, never anything else) — that every
file OneDrive actually holds matches what is on disk: same set of paths, same size, same
[QuickXorHash](../../crates/konedrived/src/quickxor.rs) (ported to Python in `quickxor.py`, with
Rust's own known-answer vectors run as a self-test on import). Matching item counts alone would
not catch a file that went up with the wrong content or arrived at the wrong name — this checks
the bytes.

**It confirms held deletes on its own** (`sync deletes confirm`, whenever this run's mass deletes
get held for confirmation, so the outbox can drain without a human in the loop) — so run it only
against a dedicated **test** account, never a real one.

## Running it

```
konedrivectl --account <test-account> account mode read-write   # once, by hand, before a run
python3 tests/stress/stress_uploads.py --account <test-account>
```

- `--account` is required — there is no "the only account there is" fallback here, unlike
  `konedrivectl` itself. The run refuses immediately, before touching anything, unless
  `account mode` for that account already says `read-write`.
- It works only inside `<folder>/konedrive-stress-<time>-<pid>/`, plus one `tempfile.mkdtemp`
  directory outside the folder (for scenario 4's move-out-and-back-in), and deletes everything it
  made at the end — the run folder itself goes to OneDrive's recycle bin along with everything in
  it, the same way any other delete does.
- Useful flags: `--files-per-folder` (default 50, so 200 files across the 4 folders),
  `--large-file-mb` (default 30), `--big-file-mb` (default 60, scenario 5's two files),
  `--outbox-timeout`, `--refresh-timeout`, `--running-timeout`, `--report <path>`. See
  `--help` for all of them.
- Exit status: 0 if every scenario passed, 1 if anything failed (including a scenario that
  errored out unexpectedly), 2 if the command line itself was wrong. The report's path is always
  printed on the last line before a normal exit.

## Scenarios

1. Create many files (default 200) across 4 folders, plus one large file (default 30 MiB).
2. Edits: append, overwrite, truncate to 0, and save-by-rename (write a temp file, then rename it
   over the original).
3. Renames, moves between folders, and a folder nested into another.
4. A file and a folder moved out of the synced folder (checked to still have their content once
   outside) and back in; a brand-new folder built outside and moved in whole.
5. A large file (default 60 MiB) moved to another folder while it is mid-upload, and a second
   large file edited while it is mid-upload — each is written, then the tool polls
   `sync outbox` until that row is `running` before touching it again. Verified the same way as
   every other scenario: after the outbox drains, OneDrive must hold exactly one copy, at the new
   path, with the latest content; the old path must not be an "extra in OneDrive" mismatch.
6. Deletes: some individual files, then a whole folder, then the whole run folder.

## The report

A Markdown file under `tests/stress/reports/` (gitignored) by default, or wherever `--report`
points. For every scenario: pass/fail and its duration. For every failure: each problem in plain
words, the relevant `sync outbox` / `sync not-uploaded` / `sync status` output at the point of
failure, any `failed` or `upload-failed` activity, and — at the end of the report —
`journalctl --user -u konedrived --since <run start> -p warning` for the whole run, so the report
is something you can paste to an LLM (or a person) without having to go dig any of that up by
hand.
