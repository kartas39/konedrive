"""Read-only verification against Microsoft Graph itself: after a scenario drains, comparing the
outbox and item counts (`stress_uploads.py`'s `finish_scenario`) proves nothing went to OneDrive
was *lost*, but it says nothing about whether what arrived is *correct* — the same byte count
with different bytes would look fine. This module lists the run folder straight from Graph and
compares every file's size and QuickXorHash (`quickxor.py`) against the local disk, and every
folder's presence, so a scenario is only a pass when OneDrive's content actually matches what is
on disk.

Every request here is a plain HTTP GET, made with `urllib.request` from the standard library —
nothing else touches the network, and nothing here can write to OneDrive. The token used is
`konedrivectl dev export-access-token`'s: read-only if Microsoft honours the read-only refresh
(`crates/konedrived/src/token.rs`'s `read_only_token`), and the run refuses to send anything but
GET with it even when the daemon falls back to a wider one.
"""

from __future__ import annotations

import json
import os
import stat
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

import quickxor

GRAPH_BASE = "https://graph.microsoft.com/v1.0"


class GraphError(Exception):
    pass


class TokenError(Exception):
    pass


def _encode_path(rel_path: str) -> str:
    segments = [s for s in rel_path.split("/") if s]
    return "/".join(urllib.parse.quote(s, safe="") for s in segments)


def _item_url(rel_path: str) -> str:
    if not rel_path:
        return f"{GRAPH_BASE}/me/drive/root"
    return f"{GRAPH_BASE}/me/drive/root:/{_encode_path(rel_path)}"


def _children_url(rel_path: str) -> str:
    if not rel_path:
        return f"{GRAPH_BASE}/me/drive/root/children?$top=999"
    return f"{GRAPH_BASE}/me/drive/root:/{_encode_path(rel_path)}:/children?$top=999"


def graph_get_json(url: str, token: str, attempts: int = 5):
    """GET `url` (which must be under GRAPH_BASE — this function is the only place in this tool
    that touches the network, and it never sends anything but GET) and return the parsed JSON
    body, or `None` for a 404 (the caller decides whether that is expected)."""
    if not url.startswith(GRAPH_BASE):
        raise GraphError(f"refusing to send a token to a non-Graph URL: {url}")
    last_error = None
    for attempt in range(attempts):
        req = urllib.request.Request(
            url, method="GET", headers={"Authorization": f"Bearer {token}", "Accept": "application/json"}
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if e.code in (429, 503, 504) and attempt < attempts - 1:
                wait = 2.0
                try:
                    wait = float(e.headers.get("Retry-After", "2"))
                except (TypeError, ValueError):
                    pass
                time.sleep(min(wait, 30.0))
                continue
            body = e.read().decode("utf-8", "replace") if e.fp else ""
            raise GraphError(f"GET {url} -> {e.code}: {body[:500]}") from e
        except urllib.error.URLError as e:
            last_error = e
            if attempt < attempts - 1:
                time.sleep(2.0)
                continue
    raise GraphError(f"GET {url} failed: {last_error}")


def get_item(rel_path: str, token: str):
    return graph_get_json(_item_url(rel_path), token)


def list_children(rel_path: str, token: str):
    """Every child of `rel_path`, following `@odata.nextLink`, or `None` if `rel_path` itself
    does not exist."""
    items = []
    url = _children_url(rel_path)
    while url:
        data = graph_get_json(url, token)
        if data is None:
            return None
        items.extend(data.get("value", []))
        url = data.get("@odata.nextLink")
    return items


def walk_subtree(rel_path: str, token: str):
    """Every file and folder under `rel_path` in OneDrive, as
    `{relative-path-from-rel_path: ("file"|"folder", size_or_None, quickxor_or_None)}`, or `None`
    if `rel_path` itself is not there (a 404 for the folder object, not just an empty listing)."""
    if get_item(rel_path, token) is None:
        return None
    result: dict = {}

    def recurse(path: str) -> None:
        children = list_children(path, token)
        if children is None:
            return
        for child in children:
            name = child.get("name", "")
            child_rel = f"{path}/{name}" if path else name
            display = child_rel[len(rel_path) + 1 :] if rel_path else child_rel
            if "folder" in child:
                result[display] = ("folder", None, None)
                recurse(child_rel)
            else:
                size = child.get("size", 0)
                hashes = (child.get("file") or {}).get("hashes") or {}
                result[display] = ("file", size, hashes.get("quickXorHash"))

    recurse(rel_path)
    return result


def local_subtree(local_root) -> dict:
    """The same shape as `walk_subtree`, computed from disk."""
    result: dict = {}
    for dirpath, dirnames, filenames in os.walk(local_root):
        rel_dir = os.path.relpath(dirpath, local_root)
        rel_dir = "" if rel_dir == "." else rel_dir.replace(os.sep, "/")
        for d in dirnames:
            rel = f"{rel_dir}/{d}" if rel_dir else d
            result[rel] = ("folder", None, None)
        for f in filenames:
            full = os.path.join(dirpath, f)
            rel = f"{rel_dir}/{f}" if rel_dir else f
            try:
                size = os.path.getsize(full)
                content_hash = quickxor.hash_file(full)
            except OSError as e:
                result[rel] = ("file", None, f"(could not read: {e})")
                continue
            result[rel] = ("file", size, content_hash)
    return result


def compare_subtree(local_root, token: str, account_relative_path: str) -> list:
    """The core check: what §step-2 of the coordinator's addition asked for. Returns one problem
    string per mismatch — missing in OneDrive, extra in OneDrive, size differs, hash differs —
    each naming the path and both values, or `[]` when the two trees agree exactly."""
    problems = []
    local_exists = os.path.isdir(local_root)
    graph_tree = walk_subtree(account_relative_path, token)
    if not local_exists:
        if graph_tree is not None:
            problems.append(
                f"{account_relative_path or '(run folder)'}: deleted locally but OneDrive still has it "
                f"({len(graph_tree)} item(s) under it)"
            )
        return problems
    if graph_tree is None:
        problems.append(f"{account_relative_path}: present locally but missing in OneDrive")
        return problems

    local_tree = local_subtree(local_root)
    for path in sorted(set(local_tree) | set(graph_tree)):
        lo = local_tree.get(path)
        gr = graph_tree.get(path)
        if lo is None:
            problems.append(f"{path}: extra in OneDrive (not present locally)")
            continue
        if gr is None:
            problems.append(f"{path}: missing in OneDrive")
            continue
        lkind, lsize, lhash = lo
        gkind, gsize, ghash = gr
        if lkind != gkind:
            problems.append(f"{path}: local is a {lkind}, OneDrive has a {gkind}")
            continue
        if lkind != "file":
            continue
        if lsize != gsize:
            problems.append(f"{path}: size differs (local {lsize} bytes, OneDrive {gsize} bytes)")
        if not ghash:
            problems.append(f"{path}: OneDrive reported no quickXorHash (local {lhash})")
        elif lhash != ghash:
            problems.append(f"{path}: quickXorHash differs (local {lhash}, OneDrive {ghash})")
    return problems


def compare_subtree_with_retry(local_root, token_getter, account_relative_path: str, retries: int = 2, delay: float = 5.0) -> list:
    """`compare_subtree`, but a hash that OneDrive has not finished computing yet for a
    just-uploaded large file is given `retries` more chances, `delay` seconds apart, before it is
    reported as a real problem — everything else is reported on the first pass."""
    problems = []
    for attempt in range(retries + 1):
        token = token_getter()
        problems = compare_subtree(local_root, token, account_relative_path)
        hash_related = [p for p in problems if "quickXorHash" in p]
        if not hash_related or attempt == retries:
            return problems
        time.sleep(delay)
    return problems


def get_read_only_token(ctl, base_tmp_dir: str) -> str:
    """A short-lived Graph token for verification only: `konedrivectl dev export-access-token`
    (no `--read-write`), which the daemon only ever hands out after a refresh scoped to
    `Files.Read` — refused, per `crates/konedrived/src/token.rs`, if Microsoft answers with
    anything wider. If that refusal happens, this falls back to `--read-write` (the only token
    available for a read-write account when Microsoft will not narrow it), but this whole module
    never sends anything but GET with whatever token it is given, so nothing here can write. The
    token file konedrivectl wrote is deleted immediately after being read, whichever path was
    used, even if reading it fails."""
    tmp_dir = tempfile.mkdtemp(prefix="konedrive-stress-token-", dir=base_tmp_dir)
    os.chmod(tmp_dir, 0o700)
    token_path = os.path.join(tmp_dir, "token")
    try:
        run = ctl.export_access_token(token_path, read_write=False)
        if not run.ok:
            run = ctl.export_access_token(token_path, read_write=True)
            if not run.ok:
                raise TokenError(
                    "konedrivectl dev export-access-token refused both a read-only and a read-write "
                    f"token: {run.combined()}"
                )
        if not os.path.isfile(token_path) or os.path.islink(token_path):
            raise TokenError("konedrivectl dev export-access-token reported success but wrote no regular file")
        mode = stat.S_IMODE(os.stat(token_path).st_mode)
        if mode != 0o600:
            raise TokenError(f"the exported token file is mode {mode:03o}, not 0600 — refusing to read it")
        with open(token_path, "r", encoding="utf-8") as f:
            token = f.read().strip()
        if not token:
            raise TokenError("the exported token file was empty")
        return token
    finally:
        try:
            if os.path.exists(token_path):
                os.remove(token_path)
            os.rmdir(tmp_dir)
        except OSError:
            pass
