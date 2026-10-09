# Security

## What runs with which rights

Nothing of KOneDrive runs as root. `konedrived`, `konedrivectl`, the `konedrive` window and both
Dolphin plugins run as your own user with no elevated privileges, and the package installs no
system service.

Each OneDrive folder is a FUSE mount the daemon serves. It is put on, and taken off, with
`fusermount3`, the setuid program of the `fuse3` package, as every FUSE filesystem of an ordinary
user is; the mount is made `nosuid` and `nodev`, and without `allow_other`, so that only your
own user reaches the folder through it.

## Credentials

Each Microsoft account's refresh token is stored in KWallet (through the Secret Service D-Bus API),
as an item of its own. The daemon reads it from KWallet to get a short-lived access token for that
account, uses that access token to talk to Microsoft Graph, and never writes the refresh token to
disk, to a log, or anywhere else. In a development build only (the `dev-tools` cargo feature,
which `scripts/dev-install.sh` uses), each account's `TokenExport` D-Bus interface hands out a
short-lived (about one hour), read-only access token of that account for test runs, whatever the
account's mode, never the refresh token; see `docs/limitations/`, W11. The
released package has neither that interface nor `konedrivectl dev`, and its build fails if the
daemon names the interface. A token that can change files (`TokenExport.ReadWrite`) is handed out
only for a test account whose drive is listed in `write_test_drive_ids` in `config.toml`, which is
all that list decides (F60); whether an account uploads is its user's own choice. Removing an account deletes its
refresh token. An upload session's URL, which lets anyone holding it write that one file until it
expires, is kept only in the account's tree store (mode `0600`), never logged and never published
over D-Bus, and the account's token is never sent to it. No token and no URL reaches a log or an
error message: every request error the daemon turns into text leaves its URL out, so a
pre-authenticated download or thumbnail link (whose query lets anyone holding it read that file
until it expires) and a redirect's `Location` never reach the journal.

## Reporting a vulnerability

Please report security issues privately, not in a public issue:

- GitHub's private vulnerability reporting: open the repository's **Security** tab and use
  **Report a vulnerability**;
- or email the maintainer at kartas39@gmail.com.

Please include what you found, how to reproduce it, and, if you can, what you think the impact
is.

There is no formal disclosure timeline or bug bounty; this is a small, alpha-stage project. You
will get a reply, and credit in the fix if you would like it.
