# Limitations

The technical limits konedrive has and cannot simply remove: the kernel, the filesystem, OneDrive
or the desktop decides, or removing the limit would give up something we chose to keep. The log is
for whoever implements: it says what does not work, why, and why it stays.

What a change in our code would remove is not a limit: it is an issue on GitHub. How the code works
belongs in the code and in `docs/design/`.

**An entry** is a file named by its id (`F53.md`) and a line here. It has a title and four short
lines: why the limit exists, what follows from it, why it stays, and where to look. No retelling of
the code. Ids do not change.

Until 2026-10-05 the log recorded every weak spot, 366 entries. An id with no file here (F245,
D33) was removed, then or since, and `git log --diff-filter=D -- docs/limitations/<id>.md` finds
its text. An id is never given twice: a new entry takes the next number its letter never had.

## What the kernel and the filesystem decide

- [F296](F296.md) — The sync folder is not served while the daemon is not running
- [F301](F301.md) — Only the user who runs the daemon reaches the sync folder
- [F124](F124.md) — A move between two accounts is a download, a removal and an upload

## What OneDrive and Microsoft decide

- [P7](P7.md) — Some OneDrive items are never shown in the folder
- [F34](F34.md) — A file OneDrive gives without a hash is downloaded unverified
- [F297](F297.md) — A range read of a file that is not on disk is not checked against the content's version
- [F39](F39.md) — An upload cannot tell OneDrive its size up front
- [F66](F66.md) — Consent to write stays with Microsoft after the account turns read-only
- [F172](F172.md) — An unfinished upload holds its name in OneDrive with an empty file
- [F295](F295.md) — "Open in OneDrive" opens under whichever personal account the browser is signed in to
- [F300](F300.md) — No links, special files or extended attributes of the user's own in the folder

## What the desktop and the package decide

- [K1](K1.md) — Dolphin opens some files just to show them, which downloads them
- [K34](K34.md) — "Move to Trash" needs KDE Frameworks 6.21 or later to delete without a download
- [W14](W14.md) — No desktop search inside a OneDrive folder
- [F175](F175.md) — No automatic pause without NetworkManager, UPower or power-profiles-daemon
- [R3](R3.md) — Removing the package does not refuse while a folder is registered

## What we chose

- [F303](F303.md) — The executable bit of a file is kept on this computer only
- [F302](F302.md) — A removal made here is sent to OneDrive with no local trash and no confirmation
- [F299](F299.md) — A directory that holds in OneDrive what the folder cannot show cannot be removed here
- [F298](F298.md) — A change made in the folder on disk while the daemon is not running is not a move or a removal in OneDrive
- [F255](F255.md) — A file that is never uploaded keeps a directory that OneDrive removed
- [D36](D36.md) — A store of another version, or a lost one, is rebuilt
- [A31](A31.md) — While a wallet prompt is open at the end of a sign-in, other account changes wait
