"""The stress tool's proof that it runs against a test account: the account's drive must be in
``write_test_drive_ids`` of the daemon's ``config.toml``, the list ``konedrive-write-test``
checks too (``tests/write-account/src/harness.rs``).

"The account is read-write" proves nothing: any signed-in account can be switched to read-write
by its user. The list is written only by hand, by the developer, for the test accounts' drives.

Only reads ``config.toml``; changes nothing, and asks the daemon nothing.
"""

from __future__ import annotations

import os
import re
from pathlib import Path

try:
    import tomllib
except ImportError:  # Python older than 3.11
    tomllib = None

ACCOUNT_ID = re.compile(r"[0-9a-f]{12}")


class NotATestAccount(Exception):
    """The account is not shown to be a test account; the message says why."""


def default_config_path() -> Path:
    """Where the daemon keeps ``config.toml`` (``crates/konedrived/src/config/paths.rs``)."""
    base = os.environ.get("XDG_CONFIG_HOME", "")
    config = Path(base) if os.path.isabs(base) else Path.home() / ".config"
    return config / "konedrive" / "config.toml"


def test_account_id(config_path: Path, wanted: str) -> str:
    """The id of the account ``wanted`` names in ``config_path``, once its recorded drive is
    found in ``write_test_drive_ids`` there. Raises ``NotATestAccount`` in every other case: the
    file missing or unreadable, no account or several by that name, no drive recorded, the list
    empty, the drive not on it.

    ``wanted`` is matched as ``konedrivectl`` matches it (``crates/konedrivectl/src/choice.rs``):
    the id exactly, or the label or the email whatever the case; the email is the one
    ``config.toml`` keeps (``login_hint``). The caller then names the account to ``konedrivectl``
    by the id returned, so that the account checked here is the one the run writes to.
    """
    if tomllib is None:
        raise NotATestAccount("this Python has no tomllib (3.11 or newer is needed) to read the daemon's config.toml")
    try:
        with open(config_path, "rb") as f:
            config = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
        raise NotATestAccount(f"the daemon's config.toml ({config_path}) cannot be read: {e}") from e

    accounts = config.get("accounts", [])
    if not isinstance(accounts, list):
        raise NotATestAccount(f"{config_path} has no list of accounts")
    name = wanted.strip()
    lower = name.lower()

    def text(account: dict, key: str) -> str:
        value = account.get(key, "")
        return value if isinstance(value, str) else ""

    named = [
        a
        for a in accounts
        if isinstance(a, dict)
        and (
            text(a, "id") == name
            or text(a, "label").lower() == lower
            or (text(a, "login_hint") and text(a, "login_hint").lower() == lower)
        )
    ]
    if len(named) != 1:
        raise NotATestAccount(
            f"{config_path} has {len(named)} accounts whose id, label or email is {wanted!r}; exactly one is needed "
            "(name it by its id, as `konedrivectl account list` shows it)"
        )
    account = named[0]
    account_id = text(account, "id")
    if not ACCOUNT_ID.fullmatch(account_id):
        raise NotATestAccount(f"the account {wanted!r} has no usable id in {config_path}")
    drive = text(account, "drive_id")
    if not drive:
        raise NotATestAccount(f"{config_path} records no OneDrive drive for the account {wanted!r}")
    listed = config.get("write_test_drive_ids", [])
    if not isinstance(listed, list) or drive not in listed:
        raise NotATestAccount(
            f"the drive of the account {wanted!r} ({drive}) is not in write_test_drive_ids in {config_path}: "
            "only a test account's drive is listed there, by hand"
        )
    return account_id
