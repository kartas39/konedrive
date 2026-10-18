# KOneDrive

A OneDrive client for KDE Plasma on Linux, with Windows' "Files On-Demand": your OneDrive is a
folder of files at their real sizes, and a file downloads when something reads it.

## Features

- **Files on demand.** Every file is there from the start and takes no space until it is read;
  then it downloads by itself, and the program that opened it just reads it.
- **Always keep on this device** and **Free up space**, for files and folders.
- **Uploads.** What you change, move or delete in the folder goes back to OneDrive. Off until you
  turn it on, account by account; until then the folder is read-only. What you delete goes to
  OneDrive's recycle bin; there is no second trash on this computer.
- **Several accounts**, each with its own folder.
- **Nothing is lost in a conflict.** A file changed on both sides is kept twice, and listed.
- **Dolphin.** An emblem on every file — online-only, downloading, downloaded, kept — and a
  right-click menu: keep, free up, open in OneDrive's web interface. Previews of images, videos, PDFs
  and office documents come from OneDrive without downloading the file.
- **A window and a tray icon** — one icon per account — with the state of each account, what is
  being transferred, recent activity, conflicts, and what could not be synced and why.
- **Pause**, for a few hours or until resumed; and by itself on a metered connection or on battery,
  if you want.
- **Notifications and progress** in Plasma, and an entry per account in Places.
- **Everything from the command line too**: `konedrivectl` does whatever the window does.
- **Nothing runs as root.** The folder is a FUSE mount the daemon serves as you.

## Screenshots

| Status | Issues | OneDrive Settings |
| --- | --- | --- |
| ![Status page](docs/screenshots/status.png) | ![Issues page](docs/screenshots/issues.png) | ![OneDrive Settings page](docs/screenshots/onedrive-settings.png) |

(Shown with example data from a test daemon, not a real OneDrive account: two accounts,
"Personal" chosen in the switcher at the top of the sidebar.)

## Status

Alpha. Built for Fedora 44 and 45 with KDE Plasma 6. Personal Microsoft accounts only; work and
school accounts are not supported yet.

## Install

Needs FUSE 3 (`fuse3`, which the package asks for), KDE Plasma 6, Qt 6.8+ and KDE Frameworks
6.21+.

Download the two RPMs of the latest release for your Fedora (`.fc44` or `.fc45` in the name) from
the Releases page — `konedrive` (the service and the command line) and `konedrive-kde` (the window
and the Dolphin plugins), not the `.src.rpm` — and install them:

```
sudo dnf install ./konedrive-*.rpm
```

Then open **KOneDrive** from the launcher, sign in and choose an empty folder. There is no package
repository yet. Upgrading, removing and the details: [the user guide](docs/user-guide.md).

## How it works

```
        the folder: a FUSE mount                D-Bus (org.konedrive.*)
┌───────────────────┐   reads, changes     ┌───────────────┐   status, actions   ┌──────────┐
│ your programs      │ ───────────────────► │ konedrived     │ ──────────────────► │ KOneDrive│
│                    │ ◄─────────────────── │ (your user)    │ ◄────────────────── │ window   │
└───────────────────┘      the files       └───────────────┘                     └──────────┘
```

- **The daemon** (`konedrived`) runs as you. It talks to Microsoft Graph, keeps a store with one
  row per item, and serves the sync folder as a FUSE mount over a backing directory: every name
  comes from the store, a file's content is fetched when it is read, and the backing directory
  holds the downloaded copies and what you made here. It sends your changes to OneDrive and
  serves the D-Bus API. Nothing runs as root. While it is not running the folder shows only what
  is on disk, and cannot be changed.
- **The window** (`konedrive`) and `konedrivectl` only talk to the daemon; closing the window does
  not stop syncing.

## Documentation

- [User guide](docs/user-guide.md) — installing, the first steps, the window, the tray, Dolphin,
  uploading, troubleshooting.
- [Command line](docs/command-line.md) — the same with `konedrivectl`.
- [Design](docs/design/README.md) — how the pieces fit together and why. It describes the
  mechanism before the mount and is being rewritten; where it differs, the code map and the code
  are right.
- [Code map](docs/code-map.md) — where everything is in the code.
- [Limitations](docs/limitations/README.md) — what does not work and cannot simply be fixed. Read
  it before filing a bug.
- [Developing](docs/developing.md) and [CONTRIBUTING.md](CONTRIBUTING.md) — building, a
  development install, the tests, the checklist for a change.
- [SECURITY.md](SECURITY.md) — what runs with which rights, the credentials, and how to report a
  vulnerability privately.

## Roadmap

1. A package repository (COPR), so that `dnf install` and `dnf upgrade` find the packages.
2. Work and school accounts (Microsoft 365, OneDrive for Business).

## License

GNU General Public License, version 3 or later (GPL-3.0-or-later). See [LICENSE](LICENSE).
