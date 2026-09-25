#!/usr/bin/env python3
"""Stress-tests konedrive's uploads by driving a real, running daemon through `konedrivectl` and
the filesystem — see `tests/stress/README.md` for what this is, why it is plain Python, and how
to run it.

It refuses to run unless `--account` names an account that is already read-write, works only
inside `<folder>/konedrive-stress-<time>/` (plus one temporary directory outside the folder, for
scenarios that move things out and, mostly, back in) and, by default, leaves everything it made in
place afterwards, locally and in OneDrive, so a run can be inspected; pass `--cleanup` for the old
behaviour of deleting it all at the end. Every scenario is checked three ways once the outbox
drains: no blocked or held rows are left, nothing of this run is stuck in `not-uploaded`, and —
after `sync refresh` — the local and OneDrive item counts agree; then, read-only against Graph
itself (`graph_check.py`), the run folder's actual content in OneDrive is compared file by file
against what is on disk (size and QuickXorHash), because matching counts alone would not catch a
file whose content went up wrong or went to the wrong name. After the fixed scenarios, a soak
phase (`--soak-minutes`, default 15) runs a seeded random mix of operations for a while and checks
once at the end, the same way.
"""

from __future__ import annotations

import argparse
import dataclasses
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
import traceback
from datetime import datetime
from pathlib import Path

import graph_check
import konedrivectl_wrap
import quickxor

FOLDER_NAMES = ["Alpha", "Bravo", "Charlie", "Delta"]


class Refused(Exception):
    """Raised before anything is written: the account is not usable for this run."""


@dataclasses.dataclass
class RunContext:
    args: argparse.Namespace
    ctl: konedrivectl_wrap.Ctl
    folder: Path
    run_root: Path
    run_root_rel: str
    outside_tmp: Path
    token_tmp_base: str
    state: dict = dataclasses.field(default_factory=dict)


@dataclasses.dataclass
class ScenarioResult:
    name: str
    passed: bool
    duration_s: float
    problems: list
    extra: dict


# -- setup and teardown --------------------------------------------------------------------------


def preflight(args: argparse.Namespace, ctl: konedrivectl_wrap.Ctl) -> RunContext:
    """Every guard this tool has before it writes anything. `--account` is required by argparse
    itself (`required=True`, below) so a run can never fall back to "the only account there is"."""
    mode_run = ctl.account_mode()
    if not mode_run.ok:
        raise Refused(f"`konedrivectl --account {args.account!r} account mode` failed: {mode_run.combined()}")
    first_line = mode_run.stdout.strip().splitlines()[0] if mode_run.stdout.strip() else ""
    mode_value = first_line.rsplit(":", 1)[-1].strip()
    if mode_value != "read-write":
        raise Refused(
            f"account {args.account!r} is not read-write (`account mode` says {first_line!r}). "
            "This tool writes real changes and confirms held deletes on its own, so it only runs "
            "against an account already switched to read-write — by hand, on a test account."
        )

    status_run = ctl.sync_status()
    if not status_run.ok:
        raise Refused(f"`sync status` failed: {status_run.combined()}")
    status = konedrivectl_wrap.parse_status(status_run.stdout)
    folder = status.get("folder", "")
    if not folder or folder == "(none)":
        raise Refused("the account has no folder registered (`sync status` shows no Folder)")
    folder_path = Path(folder)
    if not folder_path.is_dir():
        raise Refused(f"the account's folder {folder!r} does not exist on this machine")

    run_id = f"{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
    run_root = folder_path / f"konedrive-stress-{run_id}"
    if run_root.exists():
        raise Refused(f"{run_root} already exists")
    run_root.mkdir(parents=False)

    outside_tmp = Path(tempfile.mkdtemp(prefix="konedrive-stress-outside-"))
    if os.path.commonpath([str(outside_tmp), str(folder_path)]) == str(folder_path):
        raise Refused(f"the outside staging directory {outside_tmp} ended up inside the synced folder")
    token_tmp_base = tempfile.mkdtemp(prefix="konedrive-stress-tokens-")

    return RunContext(
        args=args,
        ctl=ctl,
        folder=folder_path,
        run_root=run_root,
        run_root_rel=run_root.name,
        outside_tmp=outside_tmp,
        token_tmp_base=token_tmp_base,
    )


def teardown(ctx: RunContext) -> None:
    """Local cleanup. The token temp directory is always removed (internal plumbing, nothing to
    inspect). The outside staging directory is removed only under `--cleanup`; by default it is
    left in place — it holds, among other things, scenario 5a's file, moved out while uploading and
    never moved back — and its path is printed and put in the report. The run folder itself is
    only ever removed, in OneDrive too, by scenario 6, and only under `--cleanup`."""
    try:
        os.rmdir(ctx.token_tmp_base)
    except OSError:
        pass
    if ctx.args.cleanup:
        shutil.rmtree(ctx.outside_tmp, ignore_errors=True)


# -- waiting and checking ------------------------------------------------------------------------


def wait_for_drain(ctl: konedrivectl_wrap.Ctl, timeout: float, poll: float = 1.0, stable_secs: float = 3.0):
    """Polls `sync outbox --all` until it says nothing is waiting, stable for `stable_secs`.
    Confirms any held deletes along the way — this run's own mass deletes are always held for
    confirmation, and this is the only thing in the tool that releases them (see the README: this
    is why it is for a test account only). Returns `(drained, last outbox text)`."""
    deadline = time.monotonic() + timeout
    stable_since = None
    last_text = ""
    while time.monotonic() < deadline:
        run = ctl.outbox_all()
        text = run.stdout if run.ok else last_text or run.combined()
        last_text = text
        rows = konedrivectl_wrap.parse_outbox(run.stdout) if run.ok else []
        held = [r for r in rows if r.state == "held"]
        if held:
            ctl.deletes_confirm()
            stable_since = None
            time.sleep(poll)
            continue
        if not rows:
            if stable_since is None:
                stable_since = time.monotonic()
            elif time.monotonic() - stable_since >= stable_secs:
                return True, text
        else:
            stable_since = None
        time.sleep(poll)
    return False, last_text


def poll_status_until_equal(ctl: konedrivectl_wrap.Ctl, timeout: float, poll: float = 1.0):
    """After `sync refresh`, waits for `Items: N in OneDrive, M in the folder` to agree. Returns
    `(status dict, raw text, matched)`."""
    deadline = time.monotonic() + timeout
    status: dict = {}
    text = ""
    while time.monotonic() < deadline:
        run = ctl.sync_status()
        if run.ok:
            text = run.stdout
            status = konedrivectl_wrap.parse_status(text)
            if status.get("onedrive_items") is not None and status["onedrive_items"] == status["folder_items"]:
                return status, text, True
        time.sleep(poll)
    return status, text, False


def _under_run_root(path_str: str, run_root: Path) -> bool:
    norm_path = os.path.normpath(path_str)
    norm_root = os.path.normpath(str(run_root))
    return norm_path == norm_root or norm_path.startswith(norm_root + os.sep)


def check_scenario(ctx: RunContext, tag: str = ""):
    """The four checks every scenario gets, in order: drain, no blocked/held, nothing of this run
    stuck locally, item counts agree after a refresh, and — read-only against Graph — the run
    folder's actual content matches disk. Returns `(problems, extra CLI output for the report)`."""
    problems: list = []
    extra: dict = {}
    suffix = f" ({tag})" if tag else ""

    drained, outbox_at_end = wait_for_drain(ctx.ctl, timeout=ctx.args.outbox_timeout)
    if not drained:
        problems.append(f"timed out waiting for the outbox to drain{suffix}")
        extra["outbox"] = outbox_at_end

    outbox_run = ctx.ctl.outbox_all()
    rows = konedrivectl_wrap.parse_outbox(outbox_run.stdout) if outbox_run.ok else []
    bad = [r for r in rows if r.state in ("blocked", "held")]
    if bad:
        problems.append(f"{len(bad)} row(s) left blocked or held in the outbox{suffix}")
        extra["outbox"] = outbox_run.stdout

    nu_run = ctx.ctl.not_uploaded()
    nu_items = konedrivectl_wrap.parse_not_uploaded(nu_run.stdout) if nu_run.ok else []
    ours = [item for item in nu_items if _under_run_root(item[0], ctx.run_root)]
    if ours:
        problems.append(f"{len(ours)} of this run's files are stuck in not-uploaded{suffix}")
        extra["not_uploaded"] = nu_run.stdout

    ctx.ctl.refresh()
    status, status_text, matched = poll_status_until_equal(ctx.ctl, timeout=ctx.args.refresh_timeout)
    if not matched:
        problems.append(
            f"item counts did not agree after refresh: {status.get('onedrive_items')} in OneDrive vs "
            f"{status.get('folder_items')} in the folder{suffix}"
        )
        extra["status"] = status_text

    graph_problems = graph_check.compare_subtree_with_retry(
        str(ctx.run_root),
        lambda: graph_check.get_read_only_token(ctx.ctl, ctx.token_tmp_base),
        ctx.run_root_rel,
    )
    if graph_problems:
        prefix = f"[{tag}] " if tag else ""
        problems.extend(prefix + gp for gp in graph_problems)

    if problems:
        act_run = ctx.ctl.activity(limit=200)
        if act_run.ok:
            events = konedrivectl_wrap.parse_activity(act_run.stdout)
            failed = [e for e in events if e.kind in ("failed", "upload-failed")]
            if failed:
                extra["failed_activity"] = "\n".join(
                    f"{e.at}  {e.kind}  {e.path}" + (f"  ({e.detail})" if e.detail else "") for e in failed
                )

    return problems, extra


def wait_for_outbox_state(ctl: konedrivectl_wrap.Ctl, path: Path, state: str, timeout: float, poll: float = 0.2) -> bool:
    """Polls `sync outbox` for `path`'s row to reach `state` (used by scenario 5 to catch a big
    upload while it is `running`)."""
    target = os.path.normpath(str(path))
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        run = ctl.outbox_all()
        if run.ok:
            for row in konedrivectl_wrap.parse_outbox(run.stdout):
                if row.state == state and os.path.normpath(row.path) == target:
                    return True
        time.sleep(poll)
    return False


# -- filesystem helpers ---------------------------------------------------------------------------


def write_new_file(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def random_bytes(n: int) -> bytes:
    return os.urandom(n)


# -- scenarios --------------------------------------------------------------------------------


def scenario_create_many(ctx: RunContext, problems: list, extra: dict) -> None:
    """1. create many files in 4 folders, plus one large file"""
    files_per_folder = ctx.args.files_per_folder
    tracked = {}
    for folder_name in FOLDER_NAMES:
        folder = ctx.run_root / folder_name
        folder.mkdir(parents=True, exist_ok=True)
        for i in range(files_per_folder):
            data = random_bytes(random.randint(200, 8000))
            path = folder / f"file-{i:04d}.bin"
            write_new_file(path, data)
            tracked[f"{folder_name}/file-{i:04d}.bin"] = path
    large_path = ctx.run_root / "large-file.bin"
    write_new_file(large_path, random_bytes(ctx.args.large_file_mb * 1024 * 1024))
    tracked["large-file.bin"] = large_path
    ctx.state["files"] = tracked
    extra["created"] = f"{len(tracked)} files ({files_per_folder} per folder x {len(FOLDER_NAMES)}, plus one {ctx.args.large_file_mb} MiB file)"
    p, e = check_scenario(ctx)
    problems.extend(p)
    extra.update(e)


def scenario_edits(ctx: RunContext, problems: list, extra: dict) -> None:
    """2. edits: append, overwrite, truncate to 0, save-by-rename"""
    files = ctx.state.get("files", {})
    keys = sorted(k for k in files if files[k].is_file())
    if len(keys) < 8:
        problems.append("not enough files survived from scenario 1 to run the edits scenario")
        return
    chunk = max(1, len(keys) // 4)
    groups = [keys[0:chunk], keys[chunk : 2 * chunk], keys[2 * chunk : 3 * chunk], keys[3 * chunk : 4 * chunk]]

    for k in groups[0]:
        with open(files[k], "ab") as f:
            f.write(random_bytes(random.randint(100, 2000)))
    for k in groups[1]:
        write_new_file(files[k], random_bytes(random.randint(200, 4000)))
    for k in groups[2]:
        with open(files[k], "r+b") as f:
            f.truncate(0)
    for k in groups[3]:
        p = files[k]
        tmp = p.with_name(p.name + ".tmp-savebyrename")
        write_new_file(tmp, random_bytes(random.randint(200, 4000)))
        os.rename(tmp, p)

    extra["edited"] = (
        f"appended {len(groups[0])}, overwrote {len(groups[1])}, truncated {len(groups[2])}, "
        f"save-by-rename {len(groups[3])}"
    )
    p, e = check_scenario(ctx)
    problems.extend(p)
    extra.update(e)


def scenario_renames_moves(ctx: RunContext, problems: list, extra: dict) -> None:
    """3. renames, moves between folders, a folder nested into another"""
    files = ctx.state.get("files", {})
    keys = [k for k in files if files[k].is_file()]

    rename_keys = keys[:3]
    for k in rename_keys:
        p = files[k]
        new_p = p.with_name("renamed-" + p.name)
        os.rename(p, new_p)
        files[k] = new_p

    move_keys = [k for k in files if k.startswith("Bravo/") and files[k].is_file()][:5]
    for k in move_keys:
        p = files[k]
        new_p = ctx.run_root / "Charlie" / p.name
        os.rename(p, new_p)
        files[k] = new_p

    delta_dir = ctx.run_root / "Delta"
    nested = False
    if delta_dir.is_dir():
        new_delta = ctx.run_root / "Alpha" / "Delta"
        os.rename(delta_dir, new_delta)
        nested = True
        for k, p in list(files.items()):
            try:
                rel = p.relative_to(delta_dir)
            except ValueError:
                continue
            files[k] = new_delta / rel

    extra["moved"] = f"renamed {len(rename_keys)}, moved {len(move_keys)} between folders, nested Delta into Alpha: {nested}"
    p, e = check_scenario(ctx)
    problems.extend(p)
    extra.update(e)


def scenario_move_out_and_in(ctx: RunContext, problems: list, extra: dict) -> None:
    """4. a file and a folder moved out of the folder and back in; a new folder moved in"""
    file_name = "move-out-file.bin"
    file_path = ctx.run_root / file_name
    original_data = random_bytes(random.randint(2000, 20000))
    write_new_file(file_path, original_data)

    folder_name = "MoveOutFolder"
    folder_path = ctx.run_root / folder_name
    folder_path.mkdir()
    inner_files = {}
    for i in range(5):
        data = random_bytes(random.randint(500, 3000))
        write_new_file(folder_path / f"inner-{i}.bin", data)
        inner_files[f"inner-{i}.bin"] = data

    outside_file = ctx.outside_tmp / file_name
    outside_folder = ctx.outside_tmp / folder_name
    shutil.move(str(file_path), str(outside_file))
    shutil.move(str(folder_path), str(outside_folder))

    with open(outside_file, "rb") as f:
        moved_data = f.read()
    if moved_data != original_data:
        problems.append(f"{file_name}: content changed while moved outside the folder")
    for name, data in inner_files.items():
        with open(outside_folder / name, "rb") as f:
            got = f.read()
        if got != data:
            problems.append(f"{folder_name}/{name}: content changed while moved outside the folder")

    shutil.move(str(outside_file), str(file_path))
    shutil.move(str(outside_folder), str(folder_path))

    staged = ctx.outside_tmp / "StagedNewFolder"
    staged.mkdir()
    for i in range(5):
        write_new_file(staged / f"staged-{i}.bin", random_bytes(random.randint(500, 3000)))
    shutil.move(str(staged), str(ctx.run_root / "NewFolderMovedIn"))

    extra["moved_out_and_in"] = "1 file, 1 folder of 5 files (out and back), 1 new folder of 5 files moved in"
    p, e = check_scenario(ctx)
    problems.extend(p)
    extra.update(e)


def scenario_move_while_uploading(ctx: RunContext, problems: list, extra: dict) -> None:
    """5. a file moved/renamed while uploading; another edited while uploading"""
    big_mb = ctx.args.big_file_mb

    a_path = ctx.run_root / "Alpha" / f"big-move-{big_mb}MB.bin"
    write_new_file(a_path, random_bytes(big_mb * 1024 * 1024))
    saw_running_a = wait_for_outbox_state(ctx.ctl, a_path, "running", timeout=ctx.args.running_timeout)
    if not saw_running_a:
        problems.append(f"never saw {a_path} reach 'running' in the outbox within {ctx.args.running_timeout:.0f}s")
    a_new = ctx.run_root / "Bravo" / f"big-moved-{big_mb}MB.bin"
    a_new.parent.mkdir(parents=True, exist_ok=True)
    os.rename(a_path, a_new)

    b_path = ctx.run_root / "Charlie" / f"big-edit-{big_mb}MB.bin"
    write_new_file(b_path, random_bytes(big_mb * 1024 * 1024))
    saw_running_b = wait_for_outbox_state(ctx.ctl, b_path, "running", timeout=ctx.args.running_timeout)
    if not saw_running_b:
        problems.append(f"never saw {b_path} reach 'running' in the outbox within {ctx.args.running_timeout:.0f}s")
    with open(b_path, "r+b") as f:
        f.seek(0)
        f.write(random_bytes(min(5 * 1024 * 1024, big_mb * 1024 * 1024)))

    extra["saw_running"] = f"moved while uploading: {saw_running_a}; edited while uploading: {saw_running_b}"
    p, e = check_scenario(ctx, tag="scenario 5")
    problems.extend(p)
    extra.update(e)


def scenario_move_out_while_uploading(ctx: RunContext, problems: list, extra: dict) -> None:
    """5a. a large file moved out of the folder while uploading"""
    big_mb = ctx.args.big_file_mb
    folder = ctx.run_root / "MoveOutWhileUploading"
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / f"move-out-while-uploading-{big_mb}MB.bin"
    data = random_bytes(big_mb * 1024 * 1024)
    write_new_file(path, data)
    before_hash = quickxor.hash_bytes(data)

    saw_running = wait_for_outbox_state(ctx.ctl, path, "running", timeout=ctx.args.running_timeout)
    if not saw_running:
        problems.append(f"never saw {path} reach 'running' in the outbox within {ctx.args.running_timeout:.0f}s")

    outside_path = ctx.outside_tmp / path.name
    shutil.move(str(path), str(outside_path))
    after_hash = quickxor.hash_file(str(outside_path))
    if after_hash != before_hash:
        problems.append(f"{path.name}: content changed after being moved outside the folder while uploading")

    extra["moved_out_while_uploading"] = (
        f"saw running: {saw_running}; moved to {outside_path} (left there — not moved back); "
        f"hash before move {before_hash}, after {after_hash}"
    )
    # Standard check: the file is gone from disk under run_root, so it must also be gone from the
    # live OneDrive listing (a recycle-bin copy does not show up there either way) for the two
    # trees to agree; nothing special to add beyond the usual check.
    p, e = check_scenario(ctx, tag="scenario 5a")
    problems.extend(p)
    extra.update(e)


def scenario_delete_while_uploading(ctx: RunContext, problems: list, extra: dict) -> None:
    """5b. a large file deleted while uploading"""
    big_mb = ctx.args.big_file_mb
    folder = ctx.run_root / "DeleteWhileUploading"
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / f"delete-while-uploading-{big_mb}MB.bin"
    write_new_file(path, random_bytes(big_mb * 1024 * 1024))

    saw_running = wait_for_outbox_state(ctx.ctl, path, "running", timeout=ctx.args.running_timeout)
    if not saw_running:
        problems.append(f"never saw {path} reach 'running' in the outbox within {ctx.args.running_timeout:.0f}s")

    path.unlink()

    extra["deleted_while_uploading"] = f"saw running: {saw_running}"
    p, e = check_scenario(ctx, tag="scenario 5b")
    problems.extend(p)
    extra.update(e)


def scenario_touch_before_upload(ctx: RunContext, problems: list, extra: dict) -> None:
    """5c. new files renamed or deleted before their upload starts"""
    folder = ctx.run_root / "TouchBeforeUpload"
    folder.mkdir(parents=True, exist_ok=True)
    count = 30
    paths = []
    for i in range(count):
        p = folder / f"touch-{i:03d}.bin"
        write_new_file(p, random_bytes(random.randint(200, 4000)))
        paths.append(p)

    # Deliberately no wait here — the point is to touch these before any of them has started
    # uploading.
    rename_n = count // 2
    delete_n = count // 4
    to_rename = paths[:rename_n]
    to_delete = paths[rename_n : rename_n + delete_n]
    for p in to_rename:
        os.rename(p, p.with_name("renamed-" + p.name))
    for p in to_delete:
        p.unlink()

    extra["touched_before_upload"] = (
        f"created {count}, renamed {len(to_rename)} before upload started, "
        f"deleted {len(to_delete)} before upload started, {count - len(to_rename) - len(to_delete)} left untouched"
    )
    p, e = check_scenario(ctx, tag="scenario 5c")
    problems.extend(p)
    extra.update(e)


def scenario_edit_while_queued(ctx: RunContext, problems: list, extra: dict) -> None:
    """5d. a file overwritten several times in quick succession while still queued"""
    folder = ctx.run_root / "EditWhileQueued"
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / "edited-while-queued.bin"
    write_new_file(path, random_bytes(random.randint(2000, 8000)))
    for _ in range(5):
        write_new_file(path, random_bytes(random.randint(2000, 8000)))

    extra["edited_while_queued"] = "wrote once, then overwrote 5 more times back-to-back before checking"
    p, e = check_scenario(ctx, tag="scenario 5d")
    problems.extend(p)
    extra.update(e)


def scenario_deletes(ctx: RunContext, problems: list, extra: dict) -> None:
    """6. deletes: files, then a folder (the whole run folder too, only with --cleanup)"""
    files = ctx.state.get("files", {})
    delete_keys = [k for k in list(files) if files[k].is_file()][:10]
    for k in delete_keys:
        files[k].unlink(missing_ok=True)
        del files[k]

    charlie = ctx.run_root / "Charlie"
    deleted_folder = charlie.is_dir()
    if deleted_folder:
        shutil.rmtree(charlie)

    extra["deleted"] = f"{len(delete_keys)} individual files, folder Charlie: {deleted_folder}"
    p1, e1 = check_scenario(ctx, tag="after deleting files and a folder")
    problems.extend(p1)
    for k, v in e1.items():
        extra[f"6a: {k}"] = v

    if not ctx.args.cleanup:
        extra["6b"] = "skipped (pass --cleanup to remove the run folder too) — left in place, locally and in OneDrive"
        return

    if ctx.run_root.is_dir():
        shutil.rmtree(ctx.run_root)
    p2, e2 = check_scenario(ctx, tag="after deleting the whole run folder")
    problems.extend(p2)
    for k, v in e2.items():
        extra[f"6b: {k}"] = v


SCENARIOS = [
    scenario_create_many,
    scenario_edits,
    scenario_renames_moves,
    scenario_move_out_and_in,
    scenario_move_while_uploading,
    scenario_move_out_while_uploading,
    scenario_delete_while_uploading,
    scenario_touch_before_upload,
    scenario_edit_while_queued,
    scenario_deletes,
]


# -- soak mode -------------------------------------------------------------------------------
#
# A long mixed run, on top of the fixed scenarios above: a random mix of small operations inside
# the run folder for `--soak-minutes` (default 15), so a full run takes at least about that long.
# Unlike the fixed scenarios, it does not drain and check after every operation — it runs the
# whole workload continuously, only sampling the outbox (a single, non-blocking `sync outbox`
# call) every ~30s for the report's timeline, and drains and runs the full checks (counts, and the
# Graph comparison of paths, sizes and hashes) exactly once, at the end. The disk is the truth for
# what should end up in OneDrive.

SOAK_OP_KINDS = [
    "create_small",
    "create_medium",
    "edit",
    "append",
    "truncate",
    "rename",
    "move",
    "move_out_and_back",
    "delete",
    "mkdir",
    "rmdir",
]


@dataclasses.dataclass
class SoakModel:
    """The tool's own record of what it made, so it only ever picks a real file or folder to
    touch next — never a shadow copy of file contents, since the disk itself is the truth checked
    against Graph at the end."""

    files: dict = dataclasses.field(default_factory=dict)  # rel path (from run_root) -> Path
    sizes: dict = dataclasses.field(default_factory=dict)  # rel path -> current size in bytes
    dirs: dict = dataclasses.field(default_factory=dict)  # rel path (from run_root) -> Path
    live_bytes: int = 0  # current on-disk footprint of tracked files — what the caps are checked against
    total_bytes: int = 0  # lifetime bytes written (creates + edits + appends) — for the report only
    next_file_id: int = 0
    next_dir_id: int = 0


@dataclasses.dataclass
class SoakSample:
    elapsed_s: float
    outbox_rows: int
    blocked: int
    held: int


@dataclasses.dataclass
class SoakResult:
    seed: int
    minutes: float
    duration_s: float
    op_counts: dict
    total_bytes: int
    samples: list
    passed: bool
    problems: list
    extra: dict


def _soak_rand_size(rng: random.Random, small: bool) -> int:
    return rng.randint(300, 20_000) if small else rng.randint(1 * 1024 * 1024, 5 * 1024 * 1024)


def _soak_create(ctx: RunContext, model: SoakModel, rng: random.Random, small: bool) -> bool:
    if not model.dirs:
        return False
    dir_rel = rng.choice(list(model.dirs))
    size = _soak_rand_size(rng, small)
    name = f"soak-{model.next_file_id:06d}.bin"
    model.next_file_id += 1
    data = random_bytes(size)
    path = model.dirs[dir_rel] / name
    write_new_file(path, data)
    rel = f"{dir_rel}/{name}"
    model.files[rel] = path
    model.sizes[rel] = size
    model.live_bytes += size
    model.total_bytes += size
    return True


def _soak_pick_file(model: SoakModel, rng: random.Random):
    """A random tracked file that is still really there, or `(None, None)` — dropping any tracked
    entry that turns out to be gone (moved or deleted by an earlier op this pass did not model)."""
    while model.files:
        rel = rng.choice(list(model.files))
        path = model.files[rel]
        if path.is_file():
            return rel, path
        model.live_bytes -= model.sizes.pop(rel, 0)
        del model.files[rel]
    return None, None


def _soak_edit(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None:
        return False
    size = _soak_rand_size(rng, small=True)
    data = random_bytes(size)
    write_new_file(path, data)
    model.live_bytes += size - model.sizes.get(rel, 0)
    model.sizes[rel] = size
    model.total_bytes += size
    return True


def _soak_append(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None:
        return False
    size = rng.randint(200, 5000)
    with open(path, "ab") as f:
        f.write(random_bytes(size))
    model.sizes[rel] = model.sizes.get(rel, 0) + size
    model.live_bytes += size
    model.total_bytes += size
    return True


def _soak_truncate(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None:
        return False
    with open(path, "r+b") as f:
        f.truncate(0)
    model.live_bytes -= model.sizes.get(rel, 0)
    model.sizes[rel] = 0
    return True


def _soak_rename(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None:
        return False
    new_name = f"renamed-{model.next_file_id:06d}-{path.name}"
    model.next_file_id += 1
    new_path = path.with_name(new_name)
    os.rename(path, new_path)
    dir_rel = rel.rsplit("/", 1)[0]
    new_rel = f"{dir_rel}/{new_name}"
    del model.files[rel]
    model.files[new_rel] = new_path
    model.sizes[new_rel] = model.sizes.pop(rel, 0)
    return True


def _soak_move(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None:
        return False
    cur_dir = rel.rsplit("/", 1)[0]
    candidates = [d for d in model.dirs if d != cur_dir]
    if not candidates:
        return False
    dest_dir_rel = rng.choice(candidates)
    new_path = model.dirs[dest_dir_rel] / path.name
    os.rename(path, new_path)
    new_rel = f"{dest_dir_rel}/{path.name}"
    del model.files[rel]
    model.files[new_rel] = new_path
    model.sizes[new_rel] = model.sizes.pop(rel, 0)
    return True


def _soak_move_out_and_back(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None or not model.dirs:
        return False
    outside_path = ctx.outside_tmp / f"soak-outside-{model.next_file_id:06d}-{path.name}"
    model.next_file_id += 1
    shutil.move(str(path), str(outside_path))
    dest_dir_rel = rng.choice(list(model.dirs))
    new_path = model.dirs[dest_dir_rel] / path.name
    shutil.move(str(outside_path), str(new_path))
    new_rel = f"{dest_dir_rel}/{path.name}"
    del model.files[rel]
    model.files[new_rel] = new_path
    model.sizes[new_rel] = model.sizes.pop(rel, 0)
    return True


def _soak_delete(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    rel, path = _soak_pick_file(model, rng)
    if path is None:
        return False
    path.unlink()
    model.live_bytes -= model.sizes.pop(rel, 0)
    del model.files[rel]
    return True


def _soak_mkdir(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    name = f"Dir-{model.next_dir_id:04d}"
    model.next_dir_id += 1
    path = ctx.run_root / "Soak" / name
    path.mkdir(parents=True, exist_ok=True)
    model.dirs[f"Soak/{name}"] = path
    return True


def _soak_rmdir(ctx: RunContext, model: SoakModel, rng: random.Random) -> bool:
    if len(model.dirs) <= 1:
        return False
    dir_rel = rng.choice(list(model.dirs))
    path = model.dirs.pop(dir_rel)
    for rel in [r for r in model.files if r.startswith(dir_rel + "/")]:
        model.live_bytes -= model.sizes.pop(rel, 0)
        del model.files[rel]
    if path.is_dir():
        shutil.rmtree(path)
    return True


_SOAK_OP_FUNCS = {
    "create_small": lambda ctx, model, rng: _soak_create(ctx, model, rng, small=True),
    "create_medium": lambda ctx, model, rng: _soak_create(ctx, model, rng, small=False),
    "edit": _soak_edit,
    "append": _soak_append,
    "truncate": _soak_truncate,
    "rename": _soak_rename,
    "move": _soak_move,
    "move_out_and_back": _soak_move_out_and_back,
    "delete": _soak_delete,
    "mkdir": _soak_mkdir,
    "rmdir": _soak_rmdir,
}


def _soak_choose_op(rng: random.Random, model: SoakModel, args: argparse.Namespace) -> str:
    """Picks the next kind of operation, biased away from growing the run once the soft caps
    (`--soak-max-mb`, `--soak-max-files`) are reached — checked against the *current* on-disk
    footprint, so room reopens for creates again once deletes bring it back down, instead of
    permanently starving the mix once the cap is crossed once — and restricted to operations that
    make sense when nothing (or only one directory) exists yet."""
    max_bytes = args.soak_max_mb * 1024 * 1024
    over_budget = model.live_bytes >= max_bytes or len(model.files) >= args.soak_max_files
    candidates = list(SOAK_OP_KINDS)
    if over_budget:
        candidates = [k for k in candidates if k not in ("create_small", "create_medium")]
    if not model.files:
        candidates = [k for k in candidates if k in ("create_small", "create_medium", "mkdir")]
    if not candidates:
        candidates = ["create_small"]
    return rng.choice(candidates)


def run_soak(ctx: RunContext, args: argparse.Namespace) -> SoakResult:
    seed = args.seed if args.seed is not None else random.SystemRandom().randrange(1, 2**31 - 1)
    rng = random.Random(seed)
    model = SoakModel()
    counts = {k: 0 for k in SOAK_OP_KINDS}

    (ctx.run_root / "Soak").mkdir(parents=True, exist_ok=True)
    for _ in range(4):
        name = f"Dir-{model.next_dir_id:04d}"
        model.next_dir_id += 1
        path = ctx.run_root / "Soak" / name
        path.mkdir(parents=True, exist_ok=True)
        model.dirs[f"Soak/{name}"] = path

    t_start = time.monotonic()
    deadline = t_start + args.soak_minutes * 60.0
    sample_interval = 30.0
    last_sample = t_start
    samples: list = []

    while time.monotonic() < deadline:
        op = _soak_choose_op(rng, model, args)
        if _SOAK_OP_FUNCS[op](ctx, model, rng):
            counts[op] += 1
        time.sleep(rng.uniform(0.05, 0.3))

        now = time.monotonic()
        if now - last_sample >= sample_interval:
            run = ctx.ctl.outbox_all()
            rows = konedrivectl_wrap.parse_outbox(run.stdout) if run.ok else []
            samples.append(
                SoakSample(
                    elapsed_s=now - t_start,
                    outbox_rows=len(rows),
                    blocked=sum(1 for r in rows if r.state == "blocked"),
                    held=sum(1 for r in rows if r.state == "held"),
                )
            )
            last_sample = now

    problems, extra = check_scenario(ctx, tag="soak, final check")
    duration = time.monotonic() - t_start
    return SoakResult(
        seed=seed,
        minutes=args.soak_minutes,
        duration_s=duration,
        op_counts=counts,
        total_bytes=model.total_bytes,
        samples=samples,
        passed=not problems,
        problems=problems,
        extra=extra,
    )


def run_scenario(fn, ctx: RunContext) -> ScenarioResult:
    name = (fn.__doc__ or fn.__name__).strip().splitlines()[0]
    t0 = time.monotonic()
    problems: list = []
    extra: dict = {}
    try:
        fn(ctx, problems, extra)
    except Exception:
        problems.append("unexpected error in the stress tool itself:\n" + traceback.format_exc())
    duration = time.monotonic() - t0
    passed = not problems
    print(f"[{'PASS' if passed else 'FAIL'}] {name} ({duration:.1f}s)")
    for p in problems:
        print(f"    - {p}")
    return ScenarioResult(name=name, passed=passed, duration_s=duration, problems=problems, extra=extra)


# -- report ----------------------------------------------------------------------------------


def collect_daemon_log(start_dt: datetime) -> str:
    since = start_dt.strftime("%Y-%m-%d %H:%M:%S")
    try:
        proc = subprocess.run(
            ["journalctl", "--user", "-u", "konedrived", "--since", since, "-p", "warning", "--no-pager"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except Exception as e:
        return f"(could not read the daemon's journal: {e})"
    if proc.returncode != 0:
        return f"(journalctl exited {proc.returncode}: {proc.stderr.strip()})"
    return proc.stdout.strip() or "(no warnings or errors logged during the run)"


def default_report_path(account: str) -> Path:
    base = Path(__file__).resolve().parent / "reports"
    base.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%dT%H%M%S")
    safe_account = re.sub(r"[^A-Za-z0-9_.-]+", "_", account)
    return base / f"stress-{safe_account}-{ts}.md"


def write_report(args, ctx: RunContext, results: list, soak_result, start_dt: datetime, daemon_log: str) -> Path:
    path = Path(args.report) if args.report else default_report_path(args.account)
    path.parent.mkdir(parents=True, exist_ok=True)
    end_dt = datetime.now()
    overall_ok = all(r.passed for r in results) and (soak_result is None or soak_result.passed)

    lines = [
        "# KOneDrive upload stress report",
        "",
        f"- Account: `{args.account}`",
        f"- Folder: `{ctx.folder}`",
        f"- Run folder: `{ctx.run_root}`",
        f"- Outside staging directory: `{ctx.outside_tmp}`",
        f"- Cleanup: {'done (--cleanup)' if args.cleanup else 'left in place (default; pass --cleanup to remove the run folder and the staging directory)'}",
        f"- Started: {start_dt.isoformat(timespec='seconds')}",
        f"- Finished: {end_dt.isoformat(timespec='seconds')} ({(end_dt - start_dt).total_seconds():.0f}s total)",
        f"- Result: **{'PASS' if overall_ok else 'FAIL'}**",
        "",
        "## Scenarios",
        "",
        "| # | Scenario | Result | Duration |",
        "| - | -------- | ------ | -------- |",
    ]
    for i, r in enumerate(results, 1):
        lines.append(f"| {i} | {r.name} | {'PASS' if r.passed else 'FAIL'} | {r.duration_s:.1f}s |")
    lines.append("")

    for i, r in enumerate(results, 1):
        if r.passed:
            continue
        lines.append(f"## Scenario {i}: {r.name} — FAIL ({r.duration_s:.1f}s)")
        lines.append("")
        lines.append("Problems:")
        lines.append("")
        for p in r.problems:
            lines.append(f"- {p}")
        lines.append("")
        for key, value in r.extra.items():
            lines.append(f"**{key}**")
            lines.append("")
            lines.append("```")
            lines.append(str(value).rstrip() or "(empty)")
            lines.append("```")
            lines.append("")

    if soak_result is not None:
        lines.append("## Soak")
        lines.append("")
        lines.append(f"- Seed: `{soak_result.seed}` (pass `--seed {soak_result.seed}` to replay the same operation sequence)")
        lines.append(f"- Requested: {soak_result.minutes:.0f} minute(s); actual: {soak_result.duration_s:.0f}s")
        lines.append(f"- Total bytes written: {soak_result.total_bytes} ({soak_result.total_bytes / (1024 * 1024):.1f} MiB)")
        lines.append(f"- Final check: **{'PASS' if soak_result.passed else 'FAIL'}**")
        lines.append("")
        lines.append("### Operation counts")
        lines.append("")
        lines.append("| Kind | Count |")
        lines.append("| ---- | ----- |")
        for kind in SOAK_OP_KINDS:
            lines.append(f"| {kind} | {soak_result.op_counts.get(kind, 0)} |")
        lines.append("")
        lines.append("### Outbox timeline (sampled every ~30s, without blocking)")
        lines.append("")
        lines.append("| At (s) | Outbox rows | Blocked | Held |")
        lines.append("| ------ | ----------- | ------- | ---- |")
        for s in soak_result.samples:
            lines.append(f"| {s.elapsed_s:.0f} | {s.outbox_rows} | {s.blocked} | {s.held} |")
        lines.append("")
        if not soak_result.passed:
            lines.append("### Soak final check — FAIL")
            lines.append("")
            lines.append("Problems:")
            lines.append("")
            for p in soak_result.problems:
                lines.append(f"- {p}")
            lines.append("")
            for key, value in soak_result.extra.items():
                lines.append(f"**{key}**")
                lines.append("")
                lines.append("```")
                lines.append(str(value).rstrip() or "(empty)")
                lines.append("```")
                lines.append("")

    lines.append("## The daemon's warnings and errors during the run")
    lines.append("")
    lines.append("```")
    lines.append(daemon_log)
    lines.append("```")
    lines.append("")

    path.write_text("\n".join(lines), encoding="utf-8")
    return path


def write_report_copy_into_folder(ctx: RunContext, report_path: Path) -> Path:
    """A second copy of the report, written into the account's synced folder itself (a fixed
    `konedrive-stress-reports` directory, shared across runs) so it syncs to OneDrive and stays
    there for later reference. Always called after every check this run makes, so this copy is
    never itself part of what a check compares — it also lives outside the run folder, so the
    Graph comparison (scoped to the run folder) never sees it either way."""
    dest_dir = ctx.folder / "konedrive-stress-reports"
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest_path = dest_dir / report_path.name
    dest_path.write_text(report_path.read_text(encoding="utf-8"), encoding="utf-8")
    return dest_path


# -- entry point --------------------------------------------------------------------------------


def parse_args(argv=None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Stress-test konedrive's uploads against a read-write TEST account.")
    p.add_argument(
        "--account",
        required=True,
        help="the account to run against (id, label or email, as `konedrivectl account list` shows them); "
        "must already be switched to read-write",
    )
    p.add_argument("--konedrivectl", default="konedrivectl", help="the konedrivectl binary to use (default: konedrivectl on PATH)")
    p.add_argument(
        "--files-per-folder", type=int, default=50, help="files created in each of the 4 folders in scenario 1 (default: 50, so 200 total)"
    )
    p.add_argument("--large-file-mb", type=int, default=30, help="size in MiB of scenario 1's one large file (default: 30)")
    p.add_argument("--big-file-mb", type=int, default=60, help="size in MiB of scenario 5's two files, moved/edited while uploading (default: 60)")
    p.add_argument("--outbox-timeout", type=float, default=600.0, help="seconds to wait for the outbox to drain after a scenario (default: 600)")
    p.add_argument(
        "--refresh-timeout", type=float, default=120.0, help="seconds to wait, after `sync refresh`, for the item counts to agree (default: 120)"
    )
    p.add_argument(
        "--running-timeout", type=float, default=60.0, help="seconds to wait for scenario 5's big files to show 'running' in the outbox (default: 60)"
    )
    p.add_argument("--report", default=None, help="where to write the Markdown report (default: tests/stress/reports/stress-<account>-<time>.md)")
    p.add_argument(
        "--cleanup",
        action="store_true",
        help="remove the run folder (in OneDrive too) and the outside staging directory at the end; "
        "default: leave both in place so the run can be inspected afterwards",
    )
    p.add_argument(
        "--soak-minutes",
        type=float,
        default=15.0,
        help="minutes to spend, after the fixed scenarios, running a random mix of operations inside the run "
        "folder (default: 15; 0 skips the soak phase)",
    )
    p.add_argument(
        "--seed",
        type=int,
        default=None,
        help="seed for the soak phase's RNG (default: a random seed, printed in the report so a failing run "
        "can be replayed)",
    )
    p.add_argument(
        "--soak-max-mb", type=int, default=500, help="soft cap on total bytes the soak phase writes across the whole run (default: 500)"
    )
    p.add_argument(
        "--soak-max-files", type=int, default=1500, help="soft cap on how many files the soak phase lets exist at once (default: 1500)"
    )
    args = p.parse_args(argv)
    if args.files_per_folder < 1:
        p.error("--files-per-folder must be at least 1")
    if args.large_file_mb < 1 or args.big_file_mb < 1:
        p.error("--large-file-mb and --big-file-mb must be at least 1")
    if args.soak_minutes < 0:
        p.error("--soak-minutes must not be negative")
    if args.soak_max_mb < 1 or args.soak_max_files < 1:
        p.error("--soak-max-mb and --soak-max-files must be at least 1")
    return args


def main(argv=None) -> int:
    args = parse_args(argv)
    ctl = konedrivectl_wrap.Ctl(args.konedrivectl, args.account)
    start_dt = datetime.now()

    try:
        ctx = preflight(args, ctl)
    except Refused as e:
        print(f"Refused: {e}", file=sys.stderr)
        return 1

    results = [run_scenario(fn, ctx) for fn in SCENARIOS]

    soak_result = None
    if args.soak_minutes > 0:
        print(f"Soak: running a mixed workload for {args.soak_minutes:.0f} minute(s)...")
        soak_result = run_soak(ctx, args)
        print(f"[{'PASS' if soak_result.passed else 'FAIL'}] Soak ({soak_result.duration_s:.0f}s, seed {soak_result.seed})")
        for p in soak_result.problems:
            print(f"    - {p}")

    teardown(ctx)

    daemon_log = collect_daemon_log(start_dt)
    report_path = write_report(args, ctx, results, soak_result, start_dt, daemon_log)
    print(f"Report: {report_path}")
    try:
        onedrive_report_path = write_report_copy_into_folder(ctx, report_path)
        print(f"Report (also left in the synced folder): {onedrive_report_path}")
    except OSError as e:
        print(f"(could not also write the report into the synced folder: {e})", file=sys.stderr)

    overall_ok = all(r.passed for r in results) and (soak_result is None or soak_result.passed)
    return 0 if overall_ok else 1


if __name__ == "__main__":
    sys.exit(main())
