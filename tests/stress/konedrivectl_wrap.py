"""A thin wrapper around the ``konedrivectl`` binary: run a subcommand for one account, and
parse the plain-text tables it prints (there is no ``--json``; this tool speaks the same text a
person reading ``konedrivectl --help`` would see, per ``docs/design/writes.md`` §11). The exact
strings parsed here are read out of ``crates/konedrivectl/src/lib.rs`` and
``crates/konedrived/src/tree/outbox.rs`` (the ``OutboxState`` enum: ``waiting``, ``ready``,
``running``, ``retry``, ``blocked``, ``held``), so a wording change there should update this file
too.
"""

from __future__ import annotations

import dataclasses
import re
import subprocess


@dataclasses.dataclass
class Run:
    args: list
    returncode: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0

    def combined(self) -> str:
        parts = []
        if self.stdout.strip():
            parts.append(self.stdout.rstrip())
        if self.stderr.strip():
            parts.append("(stderr) " + self.stderr.rstrip())
        return "\n".join(parts) if parts else "(no output)"


@dataclasses.dataclass
class OutboxRow:
    state: str
    kind: str
    path: str
    reason: str = ""
    raw: str = ""


@dataclasses.dataclass
class ActivityEvent:
    at: str
    kind: str
    path: str
    detail: str = ""


class Ctl:
    """Every call is `<binary> --account <account> <args...>`. Never anything else — this tool
    never picks an account implicitly, per the requirement that a stress run always names one
    explicitly."""

    def __init__(self, binary: str, account: str, timeout: float = 120.0):
        self.binary = binary
        self.account = account
        self.timeout = timeout
        self.calls = 0

    def run(self, *args: str) -> Run:
        self.calls += 1
        cmd = [self.binary, "--account", self.account, *args]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=self.timeout)
        except subprocess.TimeoutExpired as e:
            return Run(list(cmd), 124, e.stdout or "", (e.stderr or "") + f"\n(timed out after {self.timeout}s)")
        return Run(list(cmd), proc.returncode, proc.stdout, proc.stderr)

    # -- account -----------------------------------------------------------------------------

    def account_mode(self) -> Run:
        return self.run("account", "mode")

    # -- sync ----------------------------------------------------------------------------------

    def sync_status(self) -> Run:
        return self.run("sync", "status")

    def outbox_all(self) -> Run:
        return self.run("sync", "outbox", "--all")

    def not_uploaded(self) -> Run:
        return self.run("sync", "not-uploaded")

    def activity(self, limit: int = 200) -> Run:
        return self.run("sync", "activity", "--limit", str(limit))

    def refresh(self) -> Run:
        return self.run("sync", "refresh")

    def deletes_confirm(self) -> Run:
        return self.run("sync", "deletes", "confirm")

    def export_access_token(self, out_path: str, read_write: bool = False) -> Run:
        args = ["dev", "export-access-token", "--out", out_path]
        if read_write:
            args.append("--read-write")
        return self.run(*args)


# -- parsers -----------------------------------------------------------------------------------


def parse_status(text: str) -> dict:
    """`sync status`'s `Label:   value` block (`konedrivectl::sync_status_text`)."""
    fields: dict = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, _, value = line.partition(":")
        fields[key.strip()] = value.strip()
    onedrive_items = folder_items = None
    m = re.match(r"(\d+) in OneDrive, (\d+) in the folder", fields.get("Items", ""))
    if m:
        onedrive_items, folder_items = int(m.group(1)), int(m.group(2))
    blocked = 0
    m = re.match(r"(\d+)", fields.get("Blocked", ""))
    if m:
        blocked = int(m.group(1))
    held = 0
    m = re.match(r"(\d+)", fields.get("Held for confirmation", ""))
    if m:
        held = int(m.group(1))
    return {
        "folder": fields.get("Folder", ""),
        "onedrive_items": onedrive_items,
        "folder_items": folder_items,
        "blocked": blocked,
        "held": held,
        "fields": fields,
    }


_NEXT_TRY_RE = re.compile(r"  next try .+$")
_REASON_RE = re.compile(r"  \(([^)]*)\)$")
_PROGRESS_RE = re.compile(r"  \d+% of .+$")


def parse_outbox(text: str) -> list:
    """`sync outbox --all`'s rows: `{state:<8} {kind:<8} {path}[  N% of SIZE][  (reason)][  next
    try TIME]` (`konedrivectl::outbox_text`)."""
    stripped = text.strip()
    if stripped == "Nothing is waiting to upload." or not stripped:
        return []
    rows = []
    for line in text.splitlines():
        if not line.strip() or line.startswith("…") or "sync outbox --all" in line:
            continue
        if len(line) < 18:
            continue
        state = line[0:8].strip()
        kind = line[9:17].strip()
        rest = line[18:]
        m = _NEXT_TRY_RE.search(rest)
        if m:
            rest = rest[: m.start()]
        reason = ""
        m = _REASON_RE.search(rest)
        if m:
            reason = m.group(1)
            rest = rest[: m.start()]
        m = _PROGRESS_RE.search(rest)
        if m:
            rest = rest[: m.start()]
        rows.append(OutboxRow(state=state, kind=kind, path=rest, reason=reason, raw=line))
    return rows


def parse_not_uploaded(text: str) -> list:
    """`sync not-uploaded`'s `path\\n    reason\\n` pairs (`konedrivectl::not_uploaded_text`)."""
    if text.strip() == "Everything here is uploaded or waits to be.":
        return []
    items = []
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        path = lines[i]
        i += 1
        if not path.strip():
            continue
        reason = ""
        if i < len(lines) and lines[i].startswith("    "):
            reason = lines[i].strip()
            i += 1
        items.append((path, reason))
    return items


_ACTIVITY_RE = re.compile(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})\s+(\S+)\s+(.*)$")
_DETAIL_RE = re.compile(r"\s+\(([^)]*)\)$")


def parse_activity(text: str) -> list:
    """`sync activity`'s `TIME  kind  path  (detail)` lines (`konedrivectl::activity_text`)."""
    if text.strip() == "Nothing has happened yet.":
        return []
    events = []
    for line in text.splitlines():
        if not line.strip():
            continue
        m = _ACTIVITY_RE.match(line)
        if not m:
            continue
        at, kind, rest = m.groups()
        detail = ""
        dm = _DETAIL_RE.search(rest)
        if dm:
            detail = dm.group(1)
            rest = rest[: dm.start()]
        events.append(ActivityEvent(at=at, kind=kind, path=rest, detail=detail))
    return events
