# konedrive design

konedrive is a OneDrive client for KDE Plasma that reproduces Windows' *Files On-Demand* without
FUSE, without a custom filesystem and without a kernel module. The OneDrive folder is an ordinary
directory on the user's own filesystem. Every file in it is either a real file or a **placeholder**:
a sparse file with the true name, size and time, holding no data. Opening a placeholder downloads
the whole file before the open returns, so the program that opened it only ever sees the real
bytes.

This directory describes how that works and why it is built the way it is. It is written for
someone who wants to understand, review or change the system.

## Documents

| Document | What it covers |
|---|---|
| This page | The overview: the processes, the path of an open, the invariants everything else serves |
| [hydration.md](hydration.md) | Placeholders and their extended attributes, the helper and its fanotify marks, filling and freeing up files, startup recovery, the helper–daemon protocol |
| [sync.md](sync.md) | Listing the drive and following its changes, the tree store, reconciling the folder, the first listing, replacing changed files, rescues and conflicts, the read-only lock, the account |
| [pinning.md](pinning.md) | "Always keep on this device": the pin attribute, what a pin downloads and keeps, freeing up around pins, the sweep |
| [accounts.md](accounts.md) | Several accounts, each with its own folder: what an account is, one daemon and one helper link for all of them, which account an open or a path belongs to, the configuration and each account's files, keeping accounts apart, adding and removing, the move from a single-account installation |
| [desktop.md](desktop.md) | The D-Bus API, `konedrivectl`, the window and tray icon, notifications, download progress, thumbnails, Baloo, the Dolphin plugins |
| [decisions.md](decisions.md) | The notable decisions, each with its reason and its cost |
| [packaging.md](packaging.md) | The RPM packages: what goes where, why two, the helper enabled on install, upgrades, and the switch from the developer install |

Related documents elsewhere in the repository:

- [`../kernel-behavior-7.2.md`](../kernel-behavior-7.2.md) — what fanotify, leases and the
  filesystems were measured to do on Linux 7.2, and how to reproduce each measurement.
- [`../kio-behavior.md`](../kio-behavior.md) — what KIO and Dolphin open, and how the thumbnail
  cache is named.
- [`../limitations-and-workarounds.md`](../limitations-and-workarounds.md) — every known limit,
  workaround, fragile spot and provisional number. Its entries have short identifiers (Z1, P6,
  W2, F33, K1, …), which these documents use to point at them.
- [`../acceptance-check.md`](../acceptance-check.md) — a manual check of a build against a real
  account.
- [original-proposal.md](original-proposal.md) — the original proposal for
  the whole client, including the parts not built yet (uploads and write-side conflicts).
  Where it and these documents differ, these documents describe what the code does, and
  [decisions.md](decisions.md) says what changed and why.

## Status

The client is in its **read phase**. It lists the whole drive, keeps the folder in step with
changes made in the cloud, downloads on open and frees up space on request. It writes nothing to
OneDrive: the OAuth scope is `Files.Read`, so Microsoft itself refuses any write made with its
token. So that nothing local can diverge from the cloud, the folder is read-only (files `0444`,
directories `0555`); a local change forced past that lock is moved aside, never overwritten.
"Always keep on this device" (pinning) is built — see [pinning.md](pinning.md) — and so are
multiple accounts, each with its own folder — see [accounts.md](accounts.md). Uploads come later.

Supported: any number of personal Microsoft accounts, each with its own sync folder on Btrfs, ext4
or XFS; KDE Plasma 6.
The kernel needs fanotify pre-content permission events with evictable ignore marks (Linux 6.0)
and, for a denied open to carry a meaningful errno, Linux 6.14; the design was measured on 7.2.

## The processes

```text
                            Microsoft Graph (HTTPS, GET only)
                                        ▲
                                        │
 kernel ──FAN_OPEN_PERM──▶ konedrive-helper ──unix socket──▶ konedrived ◀──D-Bus──┬── KOneDrive window + tray
   (a mark on every         (root, system       + event fds    (the user,          ├── konedrivectl
    directory of the         service; no                        user service)      └── Dolphin plugins
    sync folder)             network)
```

| Process | Runs as | Does | Never does |
|---|---|---|---|
| `konedrive-helper` | root, system service, `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH` only | Owns the fanotify permission group. Marks the folder's directories, suspends opens of files that are not downloaded, hands each one to the owning user's daemon, and answers the kernel | Use the network, hold credentials, read or write file content, decide anything that needs more than an `fstat`, an `fgetxattr` and a table lookup |
| `konedrived` | the user; systemd user service, D-Bus activated | Everything else, for each of the user's accounts: sign-in and tokens, listing the drive, the tree store, placing and updating placeholders, filling and freeing up files, recovery, rescues, thumbnails, the D-Bus API | Run with any privilege |
| `konedrive` (KOneDrive) | the user | The window and tray icon: shows what the daemon publishes and calls its methods | Touch the sync folder itself |
| `konedrivectl` | the user | The command line for every feature of the window, plus developer commands | — |
| Dolphin plugins | inside Dolphin | Emblems from each file's state and pin attributes; "Always keep on this device" and "Free up space" in the context menu | Open a file in the sync folder |

The helper exists because only a fanotify group of class `FAN_CLASS_PRE_CONTENT` can hold an open
until the file has content, and creating one needs `CAP_SYS_ADMIN`. Everything that does not need
that capability is kept out of it ([decisions.md](decisions.md), "Three processes, and the root
side is minimal").

## An open, from start to finish

1. A program opens `~/OneDrive/report.pdf`, a placeholder: `user.konedrive.state` is
   `online-only`, the file is sparse and holds no data.
2. The file's directory carries a fanotify mark (`FAN_OPEN_PERM | FAN_EVENT_ON_CHILD`). The kernel
   suspends the open and queues a permission event for the helper, with a read-write descriptor of
   the file.
3. The helper reads the state through that descriptor. The file is not downloaded, so the helper
   finds the connection of the file owner's daemon, joins or creates a job for this inode (a
   hundred openers cause one download), and sends `HydrateRequest` with the descriptor over its
   Unix socket.
4. The daemon finds which of its accounts' folders holds the file ([accounts.md](accounts.md)
   §3.4), takes the file's per-inode lock, reads the state again, marks it `hydrating`, asks
   Graph, with that account's token, for the item's current metadata (size, cTag, `quickXorHash`, download URL) and streams the
   content into the file through the descriptor, hashing as it writes. A broken stream resumes with
   an HTTP `Range`; every 16 MiB the progress is made durable.
5. When every byte is written and the hash matches, the daemon sets the file's time, syncs, writes
   the stamp and, last of all, `state=hydrated`. It answers `HydrateDone`.
6. The helper reads the state once more. It says `hydrated`, so the helper places an evictable
   *ignore mark* on the file, reads the state again to be sure, and allows every waiting open.
7. The program reads the real bytes. Later opens do not reach the helper at all: the ignore mark
   suppresses the event the directory's mark would raise.

If anything fails, every waiting open is denied with an errno the kernel accepts (`EIO`, `ENOSPC`,
…) and the file goes back to `online-only`; no program is ever allowed onto a file that is not
complete. [hydration.md](hydration.md) has the details.

Changes made in the cloud travel the other way. Every 60 seconds, or at once on request, the daemon
asks Graph for the changes since its last delta link, writes them into the tree store's staging
table, makes the folder match, and only then commits the new link. [sync.md](sync.md) has the
details.

## Invariants

These properties are what the rest of the design serves. Each names where it is kept.

**An application never reads zeros where real content should be.** This outranks everything else.
A placeholder reads as zeros if anything opens it without interception, so every mechanism below
exists either to keep interception in place or to keep a file from being emptied behind its back.
The one window no design can close is the helper's death: the kernel then allows every suspended
open (limitations log Z1). That is why the helper is built so that it does not die
([hydration.md](hydration.md) §13).

**A directory is marked before content is placed in it** (M1). The kernel has no recursive marks:
a file is intercepted only because its directory carries a mark. So the daemon creates a directory
under a temporary name, has the helper mark it, and only then gives it its real name and fills it;
the helper's registration and startup walks mark every directory before descending into it.
([hydration.md](hydration.md) §3; [sync.md](sync.md) §7.3.)

**A placeholder is never opened except to fill it.** An open of a placeholder is a download, so
nothing konedrive runs opens one to look at it. The state is read from the extended attribute by
name (`lgetxattr`): by `ItemState`, by the CLI, by the Dolphin plugins. The helper works only
through descriptors it is handed and never opens a file under a marked directory. The daemon opens
a file only to fill it, free it up, recover it or update its attributes, and always beneath the
root's own descriptor. For programs outside konedrive the design removes the reasons to open:
thumbnails come from OneDrive ([desktop.md](desktop.md) §8) and the Baloo indexer is kept out of
the folder ([desktop.md](desktop.md) §9). Whatever still opens a placeholder downloads it
(limitations log P6, K1).

**No file is emptied while an ignore mark is on it** (M3). An ignore mark survives modification,
so a file emptied under one stays unintercepted and reads zeros, with nothing left to notice.
Every place that empties a file first makes the state that announces it durable (`dehydrating` or
`hydrating`), then has the helper clear the mark, and stops if it cannot.
([hydration.md](hydration.md) §3.)

**Local changes are never lost.** Before a change from the cloud touches a file, the daemon checks
that the file is still exactly what it placed or downloaded (the *stamp*: size and time when the
download completed). A file that holds local work is moved out of the way with a single rename —
never copied, never deleted — into the rescue directory, and listed as a conflict. A rename cannot
lose bytes, and a rescue that cannot be a rename fails without changing anything.
([sync.md](sync.md) §10.)

**The sync cycle is crash safe.** A crash at any point leaves a state the next run recognises and
finishes:

- the delta link advances only after the folder matches the tree it describes, in the same
  transaction that swaps that tree in;
- items being moved wait in a holding directory named by item id, so a half-done reconcile is
  resolved by id;
- the first listing commits each page together with the link to the next;
- a fill writes `state=hydrated` last, and a failed fill demotes the file before it empties it;
- startup recovery finishes whatever an interrupted fill or free-up left behind.

Losing the tree store costs one full listing, never data: the extended attributes on the files are
the truth, and the store is a map. ([sync.md](sync.md) §6, §13; [hydration.md](hydration.md) §6,
§9.)

**The root side is kept minimal.** The helper has no network (`PrivateNetwork=yes`), no tokens and
no content logic. It decides each open with an `fstat`, an `fgetxattr` and a hash lookup,
authorises every request by the peer's uid from `SO_PEERCRED` and by who owns the object, and runs
under a hardened systemd unit. Whatever can run as the user runs in the daemon.
([hydration.md](hydration.md) §11–§13; [`../../SECURITY.md`](../../SECURITY.md).)

## Where things live

| What | Where |
|---|---|
| The sync folders | one per account, anywhere the user owns, on Btrfs, ext4 or XFS; chosen at registration; never one inside another |
| Per-file state | extended attributes `user.konedrive.*` on the files and directories themselves; a OneDrive folder's root also carries its account's drive |
| Daemon configuration | `~/.config/konedrive/config.toml`: the client id, and each account with its label, mode, drive and folder ([accounts.md](accounts.md) §4.1); `config.toml.v1` after a single-account configuration was migrated |
| Tree store, activity log, conflicts | `$XDG_STATE_HOME/konedrive/accounts/<account id>/tree.sqlite` |
| Cached account name and quota | `$XDG_STATE_HOME/konedrive/accounts/<account id>/account.json` |
| Refresh tokens | the Secret Service (KWallet), one item per account; never written anywhere else |
| Rescued local changes | `$XDG_DATA_HOME/konedrive/rescued/<account id>/<time>/…`, or beside the folder when that is on another filesystem |
| The helper's registered folders | `/var/lib/konedrive/roots.json` |
| The helper's socket | `/run/konedrive/helper.sock` |
| Thumbnails | the freedesktop cache, `~/.cache/thumbnails/{normal,large,x-large}` |

## Source layout

| Path | Contents |
|---|---|
| `crates/konedrive-helper` | the privileged helper |
| `crates/konedrive-proto` | the helper–daemon wire protocol |
| `crates/konedrive-fs` | placeholder operations: extended attributes, sparse files, hole punching, leases, `O_TMPFILE`, the filesystem probe, the read-only lock's write window |
| `crates/konedrived` | the daemon: the accounts and `config.toml` with its migration, sign-in and tokens, the Graph client, the tree store, and `sync/` (the helper hub, registration, fills, recovery, listing, reconcile, replacements, pins, thumbnails, activity, D-Bus) |
| `crates/konedrive-dbus` | shared D-Bus names and error names, client proxies, the helper-state sentences |
| `crates/konedrivectl` | the command line |
| `dbus/` | the D-Bus interface definitions |
| `app/` | the KOneDrive window and tray icon (C++/QML, Kirigami) |
| `dolphin/` | the Dolphin plugins |
| `packaging/` | the systemd units and the D-Bus activation file; the RPM spec in `packaging/rpm/` |
| `scripts/` | the per-user install and its removal, the helper installer, the RPM build |
| `tests/vm/` | the privileged end-to-end suite, run in a virtme-ng VM |
| `tests/kio/` | the KIO measurements behind `docs/kio-behavior.md` |

## Glossary

- **Placeholder** — a sparse file with the item's real size and time and no data, in state
  `online-only`.
- **Hydrate, fill** — download a file's content into its placeholder. **Dehydrate, free up** —
  punch the content out again, back to `online-only`.
- **Account** — one Microsoft account signed in to konedrive, with its own folder; known by a
  random id and a label, and identified by its drive ([accounts.md](accounts.md) §2).
- **Root** — an account's registered sync folder. It carries `user.konedrive.root`, a random UUID.
- **Intercepted** — a folder whose directories the helper has marked, so that opening a
  placeholder fills it. **Without interception** is a developer's mode in which nothing does
  ([hydration.md](hydration.md) §14.3).
- **Link** — the daemon's connection to the helper, shared by every account.
- **Item id, cTag** — Graph's stable id of an item, and the tag that changes when its content
  changes.
- **Delta link** — the URL Graph hands out for asking what changed since the last listing.
- **Tree store** — the daemon's SQLite map of the drive.
- **Reconcile** — making the folder match a tree. **Full** looks at the whole folder, **Changed**
  only at the items a delta named.
- **Stamp** — the size and time a file had when its download completed; a mismatch means it was
  changed locally.
- **Rescue** — moving a file that holds local work out of the folder before a cloud change would
  replace or remove it. Each rescued file is listed as a **conflict**.
