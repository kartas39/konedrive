# KOneDrive from the command line

`konedrivectl` does everything the window does, so nothing depends on clicking through it. This
page lists the commands by task; `konedrivectl --help` and `konedrivectl <command> --help` have the
details. The window's way of doing the same things: [user-guide.md](user-guide.md).

## Choosing the account

With one account, no command needs to be told which. With several, a command that acts on one
account takes `--account <id, label or email>`, or the environment variable `KONEDRIVE_ACCOUNT`;
with neither, it stops and lists the accounts. `status` and `sync status` show every account when
none is named. The commands that take a path (`sync hydrate`, `pin`, `free`, `open`, …) find the
account from the path.

## Accounts

```
konedrivectl account add                        # sign in, in the browser; named by its email
konedrivectl account list
konedrivectl account rename bob@outlook.com Family
konedrivectl account remove Family              # says what it deleted and what it kept
konedrivectl status                             # the sign-in state
konedrivectl logout                             # sign out and delete the stored token
konedrivectl login                              # sign in again
```

- Over SSH or without a desktop: `KONEDRIVE_NO_BROWSER=1 konedrivectl account add` (or `login`)
  opens no browser and prints the address to open elsewhere.
- KOneDrive signs in with its own application registration. To use your own Microsoft Entra
  registration instead: `konedrivectl set-client-id <id>`.

## The folder

```
konedrivectl sync register ~/OneDrive           # bind an empty folder to the account
konedrivectl sync status                        # the folder, its state, the helper
konedrivectl sync refresh                       # ask OneDrive for changes now
konedrivectl sync forget                        # unbind it; the files stay as they are
```

`sync status` says how changes made in OneDrive arrive — `live`, within seconds, or
`every minute (connecting)` — and has a `Helper:` line:

- `connected` — files download when opened;
- `not-installed` — there is no konedrive-helper service on this system;
- `stopped` — installed, not running: `sudo systemctl start konedrive-helper`;
- `failed` — `systemctl status konedrive-helper` says why;
- `unknown` — systemd cannot be asked, or the helper has just started.

Right after the helper starts, `register` is refused (`NoHelper`) for up to half a minute.

## Files

```
konedrivectl sync hydrate <path>                # download one file now
konedrivectl sync pin <paths…>                  # always keep on this device
konedrivectl sync unpin <paths…>
konedrivectl sync free <paths…>                 # free up space for files or folders
konedrivectl sync free-up-space                 # ...for everything that is not in use
konedrivectl sync state <path>                  # one file's state
konedrivectl sync open <path> [--print]         # its page in OneDrive's web interface
```

## What is going on

```
konedrivectl sync activity [--limit N]          # what happened lately
konedrivectl sync transfers                     # downloads and uploads under way
konedrivectl sync conflicts                     # your versions that were kept
konedrivectl sync dismiss <path>                # take one off that list
konedrivectl sync skipped                       # in OneDrive but not in the folder, and why
```

## Uploading

```
konedrivectl account mode                       # read-only or read-write
konedrivectl account mode read-write            # turn uploading on (signs in again)
konedrivectl account mode read-only
konedrivectl sync outbox [--all]                # what waits to be uploaded, and why
konedrivectl sync not-uploaded                  # what stays on this computer, and why
konedrivectl sync ignore [list | add <pattern> | remove <pattern>]
konedrivectl sync deletes confirm | restore     # decide on a large delete held back
```

## Pausing

```
konedrivectl sync pause [--for 2h]
konedrivectl sync resume
konedrivectl sync anyway [--all]                # sync though the account paused by itself
konedrivectl settings                           # on a metered connection, on battery
```

## Other

```
konedrivectl sync thumbnails                    # whether OneDrive's thumbnails are downloaded
konedrivectl --version                          # this program's build and the service's
```

The settings that are the window's own — start at login, progress, Places, a tray icon per account
— are in `~/.config/konedriverc`, group `[General]`, and are read when the window starts.

## For scripts

When the service refuses a command, `konedrivectl` says what that means and what to do, and exits
non-zero. The refusals have D-Bus error names — `org.konedrive.Error.ModifiedLocally`,
`.NotHydrated`, `.NoHelper`, … — which is what a script calling the service directly should match.
