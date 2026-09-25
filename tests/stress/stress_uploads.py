#!/usr/bin/env python3
"""Stress-tests konedrive's uploads by driving a real, running daemon through `konedrivectl` and
the filesystem — see `tests/stress/README.md` for what this is, why it is plain Python, and how
to run it.

It refuses to run unless `--account` names an account that is already read-write, works only
inside `<folder>/konedrive-stress-<time>/` (plus one temporary directory outside the folder, for
the move-out-and-back-in scenario), and deletes everything it made at the end. Every scenario is
checked three ways once the outbox drains: no blocked or held rows are left, nothing of this run
is stuck in `not-uploaded`, and — after `sync refresh` — the local and OneDrive item counts agree;
then, read-only against Graph itself (`graph_check.py`), the run folder's actual content in
OneDrive is compared file by file against what is on disk (size and QuickXorHash), because
matching counts alone would not catch a file whose content went up wrong or went to the wrong
name.
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
    """Local cleanup only — the run folder itself is deleted, in OneDrive too, by scenario 6; this
    is just the staging directory outside the synced folder and the token temp directory."""
    shutil.rmtree(ctx.outside_tmp, ignore_errors=True)
    try:
        os.rmdir(ctx.token_tmp_base)
    except OSError:
        pass


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


def scenario_deletes(ctx: RunContext, problems: list, extra: dict) -> None:
    """6. deletes: files, a folder, then the whole run folder"""
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
    scenario_deletes,
]


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


def write_report(args, ctx: RunContext, results: list, start_dt: datetime, daemon_log: str) -> Path:
    path = Path(args.report) if args.report else default_report_path(args.account)
    path.parent.mkdir(parents=True, exist_ok=True)
    end_dt = datetime.now()
    overall_ok = all(r.passed for r in results)

    lines = [
        "# KOneDrive upload stress report",
        "",
        f"- Account: `{args.account}`",
        f"- Folder: `{ctx.folder}`",
        f"- Run folder: `{ctx.run_root}`",
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

    lines.append("## The daemon's warnings and errors during the run")
    lines.append("")
    lines.append("```")
    lines.append(daemon_log)
    lines.append("```")
    lines.append("")

    path.write_text("\n".join(lines), encoding="utf-8")
    return path


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
    args = p.parse_args(argv)
    if args.files_per_folder < 1:
        p.error("--files-per-folder must be at least 1")
    if args.large_file_mb < 1 or args.big_file_mb < 1:
        p.error("--large-file-mb and --big-file-mb must be at least 1")
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
    teardown(ctx)

    daemon_log = collect_daemon_log(start_dt)
    report_path = write_report(args, ctx, results, start_dt, daemon_log)
    print(f"Report: {report_path}")
    return 0 if all(r.passed for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
