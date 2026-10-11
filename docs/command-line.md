# KOneDrive from the command line

`konedrivectl` does everything the window does, so nothing depends on clicking through it. This
page lists the commands by task; `konedrivectl --help` and `konedrivectl <command> --help` have the
details. The window's way of doing the same things: [user-guide.md](user-guide.md).

## Choosing the account

With one account, no command needs to be told which. With several, a command that acts on one
account takes `--account <id, label or email>`, or the environment variable `KONEDRIVE_ACCOUNT`;
with neither, or with a name that fits more than one, it stops and lists the accounts. `status`
and `sync status` show every account when none is named.

The commands that take a path (`sync hydrate`, `dehydrate`, `state`, `pin`, `unpin`, `free`,
`menu`, `open`) find the account from the path. They, `account list`, `account add`,
`account rename`, `account remove`, `set-client-id`, `settings` and `sync anyway --all` refuse
`--account` and do not read `KONEDRIVE_ACCOUNT`.

## Accounts

```
konedrivectl account add                        # sign in, in the browser; named by its email
konedrivectl account list                       # id, label, email, sign-in state, mode, folder
konedrivectl account rename bob@outlook.com Family
konedrivectl account remove Family [--yes]      # says what it deleted and what it kept
konedrivectl status                             # the sign-in state
konedrivectl logout                             # sign out and delete the stored token
konedrivectl login                              # sign in again
```

- `status` prints, for an account: its label, the sign-in state, the state of the account as a
  whole, the client ID, the mode, the Microsoft account's name and email, the storage used, and
  the last error when there is one. While a switch to read-only waits, the mode line reads
  `read-write, switching to read-only: 3 changes still to send`.
- `account remove` forgets the account's folder, signs the account out and deletes its token, its
  cached name and quota, its list of OneDrive items, the activity and the conflicts list. The
  folder stays, with the files that were downloaded and your own. When changes were not sent to
  OneDrive it says how many and asks whether to go on: they stay only on this computer. `--yes`
  skips the question; without a terminal to ask on, the command fails and names `--yes`.
- Over SSH or without a desktop: `KONEDRIVE_NO_BROWSER=1 konedrivectl account add` (or `login`)
  opens no browser and prints the address to open elsewhere.
- KOneDrive signs in with its own application registration. To use your own Microsoft Entra
  registration instead: `konedrivectl set-client-id <id>`. It is refused while any account is
  signed in or signing in.

## The folder

```
konedrivectl sync register ~/OneDrive           # bind an empty folder to the account
konedrivectl sync status                        # the folder and its state
konedrivectl sync refresh                       # ask OneDrive for changes now
konedrivectl sync forget [--yes]                # unbind it; the files stay as they are
```

`sync status` prints the folder's path and state, how many items OneDrive has and how many the
folder shows, when OneDrive was last asked, how changes made in OneDrive arrive (`live`, within
seconds, or `every minute (connecting)`), the mode, what waits to be downloaded and to be
uploaded, what is blocked or waits for space in OneDrive, a pause and its cause, the space the
folder takes on this computer, the number of pins and of conflicts, and when the check of the
folder on disk at the daemon's start last finished (`Local scan:`). A line is left out when it
has nothing to say. What waits to be downloaded is every pinned file that is not on this computer
yet, counted from the moment it is pinned, and each file a program opens while it downloads; `sync
transfers` says the same number as "files left", beside how many download now.

`sync forget` takes the mount off and leaves the folder a plain directory. Changes that were not
sent to OneDrive stay as plain local files: with any, it says how many and asks whether to go on,
as `account remove` does; `--yes` skips the question.

## Files

```
konedrivectl sync hydrate <path>                # download one file now
konedrivectl sync dehydrate <path>              # free up space for one file
konedrivectl sync pin <paths…>                  # always keep on this device
konedrivectl sync unpin <paths…>                # what is downloaded stays downloaded
konedrivectl sync free <paths…>                 # free up space for files or folders; unpins them
konedrivectl sync free-up-space                 # ...for every downloaded file that is not in use
konedrivectl sync state <path>                  # one file's state
konedrivectl sync open <path> [--print]         # its page in OneDrive's web interface
konedrivectl sync menu <paths…>                 # what the file manager's menu would offer
```

## What is going on

```
konedrivectl sync activity [--limit N]          # what happened lately
konedrivectl sync transfers                     # downloads and uploads under way
konedrivectl sync conflicts                     # your versions that were kept
konedrivectl sync dismiss <path>                # take one off that list; the file stays
konedrivectl sync skipped                       # in OneDrive but not in the folder, and why
```

`sync activity` prints one line per event, newest first: the time, the kind (`downloaded`,
`freed`, `added`, `updated`, `removed`, `moved`, `uploaded`, `conflict`, `failed`, …), the full
path, and a detail in parentheses when there is one. It shows 20 events unless `--limit` says
otherwise; the daemon keeps the last 200. A directory that could not be removed here because it
holds items the folder cannot show is listed as `not-removed-here`, with what to do.

## Uploading

```
konedrivectl account mode                       # read-only or read-write
konedrivectl account mode read-write            # turn uploading on (signs in again)
konedrivectl account mode read-only [--force]   # turn it off
konedrivectl sync upload-queue [--all]          # what waits to be uploaded, and why
konedrivectl sync not-uploaded [--all]          # what stays on this computer, and why
konedrivectl sync ignore [list | add <pattern> | remove <pattern>]
```

- `account mode read-write` opens the browser, as `login` does, for Microsoft's permission to
  change the account's files, and waits. Nothing changes until the permission is granted.
- `account mode read-only` needs no sign-in. While changes made here are not sent, the account
  sends them first and switches by itself; the command says
  `Switching to read-only: 3 changes still to send`, and `account mode` and `status` show it
  until the switch is made.
- `account mode read-only --force` switches now. What was not sent stays on this computer and is
  sent when the account is read-write again.
- `account mode read-write` cancels a switch to read-only that waits.
- `sync upload-queue` prints one line per change that waits: its state (`running`, `waiting` or
  `blocked`), its kind, its path, how far an upload has got, and why it waits. It shows the first
  50 unless `--all`.
- `sync not-uploaded` prints every reason with its count, then the files of the reasons that need
  something done to each file: the first 20 of each unless `--all`.
- `sync ignore` shows or changes the names of local files that are never uploaded. A pattern is a
  shell glob matched against a name, such as `*.bak`.

A removal is sent as any other change: there is no command that confirms or takes back a large
removal. What was removed is in OneDrive's recycle bin.

## Pausing

```
konedrivectl sync pause [--for 2h]              # 30m, 2h, 1d, 1h30m; until resumed without it
konedrivectl sync resume
konedrivectl sync anyway [--all]                # sync though the account paused by itself
konedrivectl settings on-metered [pause | sync]
konedrivectl settings on-battery [sync | power-saver | pause]
```

`settings on-metered` and `settings on-battery` show the setting when no choice is given. They
hold for every account.

## Other

```
konedrivectl sync thumbnails [on | off]         # whether OneDrive's thumbnails are downloaded
konedrivectl --version                          # this program's build and the service's
```

`--version` says so when the running service is another build and should be restarted.

The settings that are the window's own — start at login, progress, Places, a tray icon per account
— are in `~/.config/konedriverc`, group `[General]`, and are read when the window starts.

A development build (`scripts/dev-install.sh`, or a package built with `--dev-tools`) also has
`konedrivectl dev add-account <label>` and `konedrivectl dev export-access-token --out <file>
[--read-write]`, for test runs. A released package has neither.

## For scripts

When the service refuses a command, `konedrivectl` says what that means and what to do, and exits
non-zero. The refusals have D-Bus error names — `org.konedrive.Error.ModifiedLocally`,
`.NotHydrated`, `.NotUp`, … — which is what a script calling the service directly should match.
A command that has a question to ask (`sync forget` and `account remove`, when changes were not
sent) fails when its input is not a terminal, unless it is given `--yes`. Answered "no" on a
terminal, it changes nothing and exits with status 1.
