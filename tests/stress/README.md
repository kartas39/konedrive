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
  directory outside the folder (for scenarios that move things out and, mostly, back in).
- By default it leaves everything it made in place afterwards, locally and in OneDrive, so a run
  can be inspected by hand — the run folder, the outside staging directory (which by design still
  holds scenario 5a's file, moved out while uploading and deliberately never moved back), and the
  report. Pass `--cleanup` for the old behaviour: scenario 6 then also deletes the whole run folder
  (which goes to OneDrive's recycle bin along with everything still in it, the same way any other
  delete does), and the staging directory is removed too.
- Useful flags: `--files-per-folder` (default 50, so 200 files across the 4 folders),
  `--large-file-mb` (default 30), `--big-file-mb` (default 60, scenario 5's and 5a/5b's files),
  `--outbox-timeout`, `--refresh-timeout`, `--running-timeout`, `--report <path>`, `--cleanup`,
  `--soak-minutes`, `--seed`, `--soak-max-mb`, `--soak-max-files`. See `--help` for all of them.
- Exit status: 0 if every scenario and the soak phase passed, 1 if anything failed (including a
  scenario that errored out unexpectedly), 2 if the command line itself was wrong. The report's
  path is always printed on the last line before a normal exit.

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
5a. A large file moved out of the synced folder entirely while it is mid-upload. The tool checks
    that it ends up absent from the live OneDrive listing (whether truly deleted or sent to the
    recycle bin — this only ever looks at the live listing, so both count as the same outcome),
    that nothing is left stuck, and that the file outside still has exactly the QuickXorHash it
    had before the move. It is left outside, in the staging directory, not moved back.
5b. A large file deleted (`rm`) while it is mid-upload. Checked the same way: absent from OneDrive
    afterwards, nothing stuck.
5c. About 30 small files, renamed (half of them) or deleted (a quarter of them) immediately, before
    any of them has had a chance to start uploading. Checked that OneDrive ends up matching disk
    exactly — renamed files only under their new names, deleted files entirely absent.
5d. A file written, then overwritten five more times back-to-back while it is still queued.
    Checked that OneDrive ends up with the last content written.
6. Deletes: some individual files, then a whole folder; the whole run folder too, only with
   `--cleanup`.

## Soak mode

After the fixed scenarios, `--soak-minutes` (default 15, so a full run takes at least about that
long) runs a random mix of small operations — create (small, and some medium, 1-5 MiB), edit,
append, truncate, rename, move between subfolders, move out to the staging directory and back in,
delete, mkdir, and rmdir of a whole subfolder — inside a dedicated `Soak/` subfolder of the run
folder, picked by a `random.Random` seeded with `--seed` (a random seed if not given, always
printed in the report so a failing run can be replayed exactly). It keeps its own record of what
it made, so it only ever touches something that is really still there; it does not drain or check
after each operation — it runs the whole workload continuously and only samples the outbox (a
single, non-blocking `sync outbox` call) every ~30s for the report's timeline. It drains and runs
the full checks (item counts, and the Graph comparison of paths, sizes and hashes) exactly once,
at the end — the disk is the truth for what should be in OneDrive. `--soak-max-mb` (default 500)
and `--soak-max-files` (default 1500) are soft caps on the current on-disk footprint of what the
soak phase has made, checked before each operation and biasing away from more creates once
reached — not a permanent cutoff, so once deletes bring the footprint back down, creates resume.

## The report

A Markdown file under `tests/stress/reports/` (gitignored) by default, or wherever `--report`
points. For every scenario: pass/fail and its duration. For every failure: each problem in plain
words, the relevant `sync outbox` / `sync not-uploaded` / `sync status` output at the point of
failure, any `failed` or `upload-failed` activity. If the soak phase ran: its seed, the count of
each kind of operation it performed, its sampled outbox timeline, and its final check's result (and
problems, if any). At the end of the report — `journalctl --user -u konedrived --since <run start>
-p warning` for the whole run, so the report is something that can be handed to whoever is
debugging a failure without having to go dig any of that up by hand.

After the final checks, the same report is also written a second time, into the account's synced
folder itself — a fixed `konedrive-stress-reports/` directory directly under `<folder>` (shared
across runs, never cleaned up), so it syncs to OneDrive and stays there for later reference. It is
written only after every check this run makes, so it is never itself part of what gets compared;
it also lives outside the run folder, so the Graph comparison (scoped to the run folder) never
sees it either way. Both paths — the local copy and the one left in the synced folder — are
printed at the end of the run.
