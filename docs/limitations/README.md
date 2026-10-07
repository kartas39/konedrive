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
D33) was removed then, and `git log --diff-filter=D -- docs/limitations/<id>.md` finds its text.

## What the kernel and the filesystem decide

- [P2](P2.md) — Opening a file while it is leased fails with "operation not permitted"
- [Z1](Z1.md) — When the helper crashes or is killed, every waiting open reads zeros; while none runs, nothing is intercepted
- [Z2](Z2.md) — A placeholder moved into a just-created directory can read zeros
- [Z3](Z3.md) — A hard link or a move out of the folder escapes interception
- [Z5](Z5.md) — Freeing a file's space while it is still exempt leaves zeros forever
- [F53](F53.md) — A move is told from a copy only by the recorded inode
- [F54](F54.md) — A local delete reaches OneDrive only when the helper proves it
- [F72](F72.md) — A subvolume or mount inside the folder is not synced
- [F74](F74.md) — Every change in a read-write folder must come from the daemon process
- [F75](F75.md) — A truncate by path and a write through a mapping are not seen
- [F120](F120.md) — A placeholder moved out of the folder reads zeros until marked again
- [F121](F121.md) — A move out is finished only when the object can be reached
- [F187](F187.md) — A program holding a file open can lose writes when OneDrive removes it
- [F193](F193.md) — A file saved by rename and moved at once loses its history
- [F261](F261.md) — After the folder's filesystem changes, an unfinished move out is given up
- [F16](F16.md) — A rescue fails when the sync folder is itself a mount point

## What OneDrive and Microsoft decide

- [P7](P7.md) — Some OneDrive items are never placed in the folder
- [F34](F34.md) — A file OneDrive gives without a hash is downloaded unverified
- [F39](F39.md) — An upload cannot tell OneDrive its size up front
- [F66](F66.md) — Consent to write stays with Microsoft after the account turns read-only
- [F80](F80.md) — A finishing upload can replace an edit just made in OneDrive
- [F124](F124.md) — A move between two accounts is a download, a delete and an upload
- [F172](F172.md) — An unfinished upload holds its name in OneDrive with an empty file
- [F295](F295.md) — "Open in OneDrive" opens under whichever personal account the browser is signed in to

## What the desktop and the package decide

- [K1](K1.md) — Dolphin opens some files just to show them, which downloads them
- [W14](W14.md) — No desktop search inside a OneDrive folder
- [F175](F175.md) — No automatic pause without NetworkManager, UPower or power-profiles-daemon
- [R3](R3.md) — Removing the package does not refuse while a folder is registered

## What we chose

- [P6](P6.md) — Anything that opens a placeholder downloads the whole file
- [F47](F47.md) — Removing an account leaves undownloaded files as empty placeholders
- [F140](F140.md) — A read-only folder with changes waiting to upload stops following OneDrive
- [F255](F255.md) — A local-only file keeps a folder that OneDrive no longer has
- [F38](F38.md) — Finding pins means walking the whole folder
- [F43](F43.md) — All accounts of one user share one link to the helper
- [F208](F208.md) — The helper caps each user at 32 folders and 8192 waiting opens
- [D36](D36.md) — An older build rebuilds a store written by a newer build
- [A31](A31.md) — While a wallet prompt is open at the end of a sign-in, other account changes wait
