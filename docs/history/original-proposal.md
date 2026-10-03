# Linux OneDrive Client — Design Document

The original proposal.  
What was built, and why it differs, is in README.md and decisions.md.

**Status:** Proposed design  
**Target environment:** Fedora / KDE Plasma / Dolphin, modern Linux 7.2.x+  
**Primary goal:** Reproduce the practical Windows OneDrive *Files On-Demand* experience on Linux without FUSE, without a custom filesystem, and without a kernel module.

---

## 1. Goals

The client should behave as close as practical to OneDrive on Windows:

- A normal local directory such as `~/OneDrive-Work`.
- Files and directories appear immediately even when file content is not stored locally.
- Online-only files show their **real logical file size**.
- Opening an online-only file transparently downloads the **whole file** before the original open is allowed to proceed.
- Downloaded files behave as completely normal files on the underlying filesystem.
- User can mark a file or directory as **Always keep on this device**.
- User can mark a downloaded file as **Free up space**, returning it to online-only state without changing its logical size or pathname.
- Remote changes arrive locally automatically.
- Local changes upload automatically.
- The file manager shows per-file synchronization state with overlay icons.
- Filesystem navigation and status display must remain fast and must never depend on a network round trip.
- The design must survive daemon crashes, network failures, reboots, conflicting edits, and interrupted transfers without silently losing user data.

### Non-goals

- No partial/range hydration. Hydration is always whole-file.
- No FUSE-based remote mount.
- No custom filesystem implementation.
- No custom kernel module unless a later prototype proves the userspace design inadequate.
- No attempt to provide perfect compatibility with every possible Linux filesystem initially. The primary target is Btrfs on Fedora; ext4 can be supported where required facilities are available.

---

## 2. High-level architecture

```text
                         Microsoft Graph / OneDrive
                                  ↑   ↓
                         ┌─────────────────┐
                         │    onedrived    │
                         │                 │
                         │ Graph client    │
                         │ delta sync      │
                         │ upload engine   │
                         │ download engine │
                         │ conflict logic  │
                         │ state machine   │
                         │ SQLite index    │
                         │ xattr manager   │
                         └───────┬─────────┘
                                 │ IPC
                                 │
                    ┌────────────┴────────────┐
                    │                         │
        privileged fanotify helper       KDE integration
                    │                         │
                    ↓                         ↓
             normal Btrfs/ext4          Dolphin overlays,
             ~/OneDrive-Work            context actions,
                                        notifications
```

There are three implementation components:

1. **`onedrive-fanotify-helper`** — very small privileged filesystem-event/interception process.
2. **`onedrived`** — unprivileged per-user synchronization daemon containing almost all product logic.
3. **`onedrive-kde`** — Dolphin/KDE integration for overlay icons and commands.

The OAuth/Graph credentials belong only to the unprivileged user daemon. The privileged helper must not have Microsoft credentials or implement cloud protocol logic.

---

## 3. Local filesystem model

The OneDrive directory is an **ordinary directory on the user's real local filesystem**:

```text
/home/user/OneDrive-Work/
    Documents/
    Projects/
    Photos/
```

It is not a mount point and not a virtual FUSE view.

### 3.1 Hydrated file

A hydrated file is simply a normal local file:

```text
report.pdf
logical size:    84 MB
allocated:       ~84 MB
state:           hydrated + synced
```

Once hydrated, ordinary I/O follows the normal Linux path:

```text
application
    ↓
Linux VFS
    ↓
Btrfs/ext4
    ↓
page cache / SSD
```

The sync daemon is not in the normal read path after hydration.

### 3.2 Online-only placeholder

An online-only item is represented as a **real sparse file** with the correct logical size but with no payload blocks allocated locally.

Example:

```text
movie.mkv
logical size (`st_size`):  4.7 GB
allocated blocks:          approximately 0
state:                     online-only
```

Dolphin, `ls -l`, file dialogs and applications therefore see the real 4.7 GB file size even before hydration.

The payload is not represented by a second visible file and no separate virtual namespace exists.

### 3.3 Placeholder creation

For a remote file with size `N`:

1. Create a local file.
2. Set its logical length to `N` while leaving it sparse.
3. Set its OneDrive identity and state metadata.
4. Insert/update the object in the local index.

A genuine zero-byte OneDrive file is distinguishable because it carries OneDrive identity/state metadata even though `st_size == 0`.

---

## 4. Sync-root binding

A local directory must be explicitly associated with one Microsoft drive/root.

Example:

```text
~/OneDrive-Personal  → account A / drive X / remote root R1
~/OneDrive-Work      → account B / drive Y / remote root R2
```

The persistent sync-root record contains at least:

```text
root_id
local_path
account_id
tenant_id (if applicable)
drive_id
remote_root_item_id
```

The root directory should additionally carry a small recovery xattr such as:

```text
user.onedrive.root-id
```

This makes the relationship between a local tree and the daemon configuration explicit rather than inferred from a directory name.

---

## 5. Per-file metadata: xattrs

Metadata intrinsically associated with a particular filesystem object should live with that object in extended attributes.

Recommended minimum set:

```text
user.onedrive.item-id
user.onedrive.etag
user.onedrive.state
user.onedrive.pin
```

Possible states include:

```text
online-only
hydrating
hydrated
local-dirty
uploading
dehydrating
conflict
error
```

Possible pin values include:

```text
none
pinned
recursive
```

A recursive pin on a directory is a **policy**, not merely a statement about current contents. New descendants created remotely under that directory inherit the requirement to remain local.

### Why xattrs are useful

- Metadata follows an inode across a normal rename within the same filesystem.
- The filesystem object remains self-describing.
- The local database can be reconstructed to a useful degree if needed.
- Dolphin/status code can reason about an individual item without requiring its remote path to be embedded in the filename hierarchy.

### Why xattrs are not the only state store

xattrs are poor as a global index. Given a OneDrive `item-id`, locating the corresponding inode would otherwise require scanning the filesystem tree. They are also insufficient as the only durable transaction journal.

---

## 6. SQLite: index and transaction journal

SQLite should be used, but **not as the sole canonical representation of every file attribute**.

Its primary roles are:

- remote ID → local object/path lookup;
- durable operation journal;
- Microsoft Graph delta state;
- pending uploads/deletes/moves;
- conflict bookkeeping;
- crash recovery;
- fast queries for UI/status purposes.

Suggested logical tables:

```text
accounts
sync_roots
items_index
pending_operations
delta_state
conflicts
```

A minimal item index may contain:

```text
drive_id
item_id
parent_item_id
local_identity
cached_path
```

The exact representation of `local_identity` should be chosen carefully. A raw inode number alone is not globally permanent across deletion/recreation, so it should be paired with filesystem/device identity and treated as an optimization rather than as remote identity.

### Why a database is still valuable

Microsoft Graph delta explicitly recommends tracking DriveItems by `id`, not by path. A parent folder rename may not cause all descendants to appear again in the delta feed, and `parentReference.path` may be omitted. Therefore a fast `item-id → local object` index is useful.

SQLite also provides atomic transactions for state changes such as:

```text
local-dirty → uploading → synced
online-only → hydrating → hydrated
hydrated → dehydrating → online-only
```

The database complements xattrs; it does not replace them.

---

## 7. Remote change tracking

Use Microsoft Graph `driveItem: delta`.

### Initial synchronization

1. Call delta for the configured remote root/drive.
2. Follow all `@odata.nextLink` pages.
3. Create/update local directories and sparse placeholders.
4. Apply remote deletes/moves/renames.
5. Only after the whole change set is committed locally, persist the returned `@odata.deltaLink`.

### Incremental synchronization

Subsequent polls use the saved delta link/token and process only remote changes.

Important Graph behavior:

- Delta represents the latest state, not an event log containing every intermediate rename.
- The same item can appear multiple times; the final occurrence wins.
- Items must be tracked by remote `id`, not by path.
- Renaming a directory does not imply all descendants are returned with new paths.

This avoids repeated remote tree scans.

---

## 8. On-demand hydration

Hydration is whole-file only.

### 8.1 Interception mechanism

For an online-only placeholder, the system must stop an application before it obtains usable access to unsatisfied file content.

The conservative first implementation uses `fanotify` permission events, primarily `FAN_OPEN_PERM`, from a `FAN_CLASS_PRE_CONTENT`/permission-capable listener.

Conceptual flow:

```text
application calls open("report.docx")
                ↓
              kernel
                ↓
          FAN_OPEN_PERM
                ↓
      fanotify helper / daemon
                ↓
       state == online-only?
          /               \
        no                 yes
        ↓                   ↓
    FAN_ALLOW          hydrate fully
                            ↓
                         fsync
                            ↓
                    state=hydrated
                            ↓
                       FAN_ALLOW
                ↓
       original open continues
```

`fanotify` permission events require the listener to reply with an allow/deny decision; therefore the original open can be held while hydration occurs.

### 8.2 Whole-file hydration

No range/page hydration is performed.

Benefits:

- drastically simpler state model;
- no sparse subrange bookkeeping;
- avoids needing page-fault-level cloud callbacks;
- subsequent `mmap()` or ordinary reads see a normal local file;
- after hydration there is no cloud layer in the normal I/O path.

### 8.3 In-place hydration

The placeholder must be populated **without replacing the filesystem object being handled by the blocked operation**.

The fanotify event can expose an fd to the monitored object; `fanotify_init()` can request event fds with `O_RDWR`, and fanotify event fds carry `FMODE_NONOTIFY`, suppressing recursive fanotify generation for accesses through that event fd.

Therefore the desired sequence is:

```text
placeholder inode/object
    ↓
write downloaded payload into same object
    ↓
fsync
    ↓
update state metadata
    ↓
allow original open
```

Do not hydrate by downloading another inode and simply renaming it over the placeholder while the original permission event is waiting.

### 8.4 Failure handling

If hydration fails:

- do not mark the file hydrated;
- leave/recover it as online-only or explicit error state;
- deny or fail the original operation with a useful error;
- retain a durable diagnostic/retry record.

If the daemon crashes while state is `hydrating`, startup recovery must treat the payload as potentially incomplete and revalidate/re-hydrate it.

---

## 9. Dehydration — “Free up space”

The user action should turn a clean hydrated file back into an online-only sparse placeholder while preserving:

- pathname;
- logical file size;
- OneDrive identity;
- timestamps where appropriate;
- synchronization identity/state.

Proposed sequence:

```text
hydrated
    ↓
verify file is clean and remote version is safely stored
    ↓
state = dehydrating
    ↓
release local data blocks / punch hole while preserving st_size
    ↓
fsync relevant metadata
    ↓
state = online-only
```

The exact Btrfs/ext4 hole-punch behavior must be validated in the prototype before being relied upon for production data.

A dirty local file must never be dehydrated until its content is uploaded or the user explicitly resolves/discards the local change.

---

## 10. “Always keep on this device”

Pinning is persistent policy.

### File pin

```text
pin = pinned
```

The daemon guarantees that the file becomes and remains hydrated.

### Directory pin

```text
pin = recursive
```

The daemon must:

- hydrate existing descendants;
- hydrate new remote descendants automatically;
- refresh locally stored content when remote versions change;
- preserve the policy across restarts.

Unpinning removes the policy but does not necessarily dehydrate immediately unless the user selects **Free up space**.

---

## 11. Local change detection and upload

A separate notification path should detect local changes. This can use a non-blocking fanotify notification group and/or other Linux filesystem notifications as appropriate.

Relevant classes of events include:

```text
create
close-after-write
rename/move
delete
metadata changes where relevant
```

A local content change results in:

```text
hydrated + synced
      ↓
local write closes
      ↓
state = local-dirty
      ↓
durable pending upload record
      ↓
upload to Graph
      ↓
receive new remote identity/version metadata
      ↓
update xattr + DB
      ↓
state = hydrated + synced
```

For large files, use Microsoft Graph upload sessions so an interrupted transfer can resume rather than restarting from byte zero.

---

## 12. Conflict detection

Silent last-writer-wins is unacceptable.

Store the last known remote ETag/version. Before committing an upload of a previously existing file, use optimistic concurrency (`If-Match` where supported by the Graph operation).

If the remote ETag changed since the local version was based on it, treat the operation as a conflict rather than overwriting remote content.

Example handling:

```text
foo.docx
foo (conflicted copy 2026-09-22).docx
```

or expose a user-visible conflict requiring manual resolution.

The key invariant is:

> No version is silently discarded when both local and remote changed independently.

---

## 13. Remote changes to locally present files

### Remote change, local file clean

The new remote content may be downloaded and applied safely, preserving the file's sync identity.

Care is required if another process currently has the file open. The implementation must define and test replacement/update semantics rather than blindly overwriting an in-use file.

### Remote change, local file dirty

Do not overwrite.

Mark conflict and retain both versions or require explicit resolution.

### Remote delete, local file clean

Remove the local object after journal/state commit.

### Remote delete, local file dirty

Treat as conflict. The local unsynchronized user data must not be silently deleted.

---

## 14. Crash consistency

Every destructive or state-changing operation must be represented by an explicit durable transition.

Examples:

```text
online-only
  → hydrating
  → hydrated

hydrated/synced
  → local-dirty
  → uploading
  → hydrated/synced

hydrated/synced
  → dehydrating
  → online-only
```

The durable journal/state transition must occur before an operation enters a state from which recovery would otherwise be ambiguous.

At daemon startup:

1. Inspect unfinished journal entries.
2. Reconcile DB with xattrs/filesystem objects.
3. Verify any object left in transient states.
4. Retry or safely roll back interrupted operations.
5. Resume remote delta processing only when the local state is consistent enough to do so.

---

## 15. Privilege separation

Permission/pre-content fanotify functionality requires elevated capability in configurations relevant to this design. The cloud daemon should nevertheless remain unprivileged.

Recommended separation:

```text
onedrive-fanotify-helper
  - privileged / narrowly scoped
  - owns fanotify permission group
  - passes filesystem events/handles over local IPC
  - accepts narrowly defined hydrate/allow/deny coordination
  - contains no OAuth token
  - contains no Graph implementation

onedrived
  - runs as user
  - owns OAuth credentials
  - performs Graph operations
  - owns SQLite DB
  - owns sync state machine
```

The privileged helper should be intentionally tiny and defensive because it sits in a filesystem permission path.

A prototype may initially run a combined process with appropriate privilege purely to validate kernel behavior, but that is not the desired final security model.

---

## 16. KDE / Dolphin integration

### 16.1 Overlay icons

KF6 provides `KOverlayIconPlugin`, which allows file managers to add overlay icons to files.

Suggested states:

```text
☁   online-only
✓   downloaded + synced
✓●  pinned / always local
↻   syncing
!   error or conflict
```

The exact artwork should use KDE theme-compatible icons rather than Unicode glyphs in the actual plugin.

`KOverlayIconPlugin::getOverlays()` is called from the main thread and must not block. Therefore **the plugin must never contact Microsoft Graph**.

Status path:

```text
Dolphin
   ↓
KOverlayIconPlugin
   ↓
local status cache / non-blocking IPC
   ↓
onedrived
```

The plugin should maintain a small local cache and update icons through `overlaysChanged()` when daemon state changes.

### 16.2 Context actions

Dolphin should expose actions equivalent to Windows OneDrive:

```text
Download now
Always keep on this device
Free up space
Sync now
View online
```

These actions call `onedrived` over D-Bus or another local IPC mechanism.

### 16.3 Notifications

KDE notifications may be used for:

- synchronization errors;
- authentication expiration;
- conflicts;
- quota failures;
- optionally long-running hydration/upload progress.

Normal successful background synchronization should remain quiet.

---

## 17. IPC

D-Bus is a natural fit for the KDE/user-service boundary.

Possible user-daemon API:

```text
GetStatus(path)
GetStatuses(paths[])
Pin(path, recursive)
Unpin(path)
Hydrate(path)
Dehydrate(path)
SyncNow(path/root)
GetSyncRoot(path)
GetErrors(root)
```

For performance, Dolphin should prefer batched status calls and a local cache rather than one synchronous IPC call per visible file.

The privileged fanotify helper may use a dedicated Unix domain socket/private protocol instead of exposing a broad D-Bus surface.

---

## 18. Performance principles

The project exists partly because existing Linux OneDrive/FUSE experiences can feel slow. The design therefore follows strict rules:

### Never put the network on the directory-navigation path

These must be local-only operations:

```text
readdir / directory listing
stat / size lookup
icon/status lookup
rename within local tree
ordinary reads of hydrated content
```

### Network operations are asynchronous except for intentional hydration

The only normal user operation allowed to block on network by design is opening/accessing content that is currently online-only.

### Hydrated files are ordinary files

Once a file is local, read performance should be that of the underlying Btrfs/ext4 file, page cache and SSD rather than a userspace proxy filesystem.

### Remote tree scans are avoided

Use Graph delta tokens instead of repeatedly enumerating the entire remote hierarchy.

### Global local scans are exceptional

SQLite provides a fast remote-ID/local-object index and operation journal. Full local scans are for recovery/reconciliation, not the steady-state hot path.

---

## 19. Important weak points and mitigations

### 19.1 Opening can hydrate more eagerly than strictly necessary

`FAN_OPEN_PERM` triggers on open, not necessarily on a later content read. A thumbnailer, indexer, antivirus-like scanner or other program can therefore cause hydration.

This behavior is acceptable for the first implementation and is conceptually similar to desktop cloud clients where previews/indexing can cause online files to download.

Do not prematurely add process-specific bypass rules. First measure actual KDE/Baloo/thumbnailer behavior.

### 19.2 fanotify/HSM semantics must be validated on the exact target kernel/filesystem

Linux 7.2.x is the current target generation, but kernel/storage behavior must be verified by a PoC on the actual Fedora kernel and Btrfs version.

Tests must include:

- normal `open/read`;
- open followed by `mmap`;
- multiple concurrent opens of one online-only file;
- process cancellation while hydration is pending;
- daemon/helper crash while an open is blocked;
- renames during/around hydration;
- deletes during/around hydration;
- very large sparse files;
- files with unusual names;
- filesystem full during hydration;
- network loss mid-hydration;
- suspend/resume;
- logout/reboot.

### 19.3 In-place hydration creates a crash window

The file may temporarily contain a partially downloaded payload. This is why `hydrating` must be durable and the original open must not be allowed until the full content has been written and flushed.

A checksum/hash may be used where Graph metadata provides a suitable value, but correctness must not depend on a checksum being available for every OneDrive type/account.

### 19.4 xattrs can be lost by external tools

Some copy/archive/backup workflows do not preserve xattrs. The SQLite index and remote state provide recovery assistance.

A copied file outside the sync root should normally become an ordinary independent local file, so losing OneDrive xattrs in that scenario is often desirable.

### 19.5 Path is not identity

Never use pathname as the primary remote identity. OneDrive `DriveItem.id` is the stable cloud identity; local path is derived/cacheable state.

### 19.6 Local inode is not remote identity either

An inode can disappear and later be reused. Pair local object tracking with filesystem identity and remote ID, and revalidate after destructive changes.

### 19.7 Queue overflow / missed local events

Filesystem event streams must not be the sole source of truth. On overflow, uncertain shutdown, or detected inconsistency, perform targeted or full reconciliation.

### 19.8 Authentication and Graph throttling

The daemon must handle:

- token refresh;
- revoked consent;
- account sign-out;
- Graph throttling / `Retry-After`;
- temporary HTTP/service failures;
- offline operation.

None of these should make local hydrated files unusable.

---

## 20. Multiple accounts and SharePoint

The architecture naturally supports multiple sync roots because identity includes both drive/account and DriveItem ID.

Examples:

```text
~/OneDrive-Personal
~/OneDrive-Work
~/SharePoint-Project-A
```

A move within one sync root is normally a remote move/rename.

A move between different sync roots/drives is logically a cross-provider/cross-drive operation and should be treated as a local copy/upload into the destination plus deletion from the source once safe, rather than pretending the remote object identity survives.

---

## 21. Suggested internal state model

A simple conceptual state machine:

```text
                  remote metadata discovered
                           ↓
                     ONLINE_ONLY
                       /      \
            explicit/open      pin
                 hydration      │
                       \        /
                        HYDRATING
                            ↓
                         HYDRATED
                            ↓
                  local modification
                            ↓
                       LOCAL_DIRTY
                            ↓
                         UPLOADING
                       /           \
                   success        conflict/error
                     ↓                 ↓
                 HYDRATED          CONFLICT/ERROR

HYDRATED + clean
      ↓ Free up space
 DEHYDRATING
      ↓
 ONLINE_ONLY
```

Transient states must be journaled.

---

## 22. First implementation milestones

### Phase 1 — Kernel/filesystem PoC

Prove only the fundamental mechanism:

1. Create a sparse placeholder with a real logical size.
2. Mark it online-only.
3. Intercept open with fanotify permission event.
4. Copy a known local test payload into the same filesystem object.
5. `fsync`.
6. Allow the blocked open.
7. Verify the caller receives correct content.
8. Repeat with concurrent access and `mmap` after open.

No Graph, no KDE, no OAuth.

### Phase 2 — Minimal OneDrive sync

- Microsoft login/authentication.
- One sync root.
- initial Graph delta enumeration;
- placeholders;
- on-demand full hydration;
- local upload;
- simple delete/rename handling;
- durable state journal.

### Phase 3 — Windows-like file states

- pin/unpin;
- recursive directory pin;
- free-up-space/dehydration;
- robust conflicts;
- resumable uploads;
- crash recovery.

### Phase 4 — KDE integration

- `KOverlayIconPlugin`;
- context actions;
- status/progress notifications;
- batched cached status IPC.

### Phase 5 — Hardening

- multi-account;
- SharePoint document libraries;
- throttling/retry policy;
- large-tree testing;
- corruption/recovery tooling;
- automated fault injection;
- security review of privileged helper.

---

## 23. Design decisions explicitly rejected

### FUSE remote filesystem

Rejected as the primary architecture because it creates a separate virtual namespace/data path and makes every file operation pass through userspace filesystem machinery. It also makes the underlying physical storage representation differ from the visible namespace.

### Range hydration

Rejected. Whole-file hydration is simpler and matches the desired behavior.

### Custom kernel filesystem/module

Not justified unless the fanotify-based PoC demonstrates a fundamental blocker. Modern fanotify provides the required permission/pre-content primitives for a userspace HSM/cloud-storage style implementation.

### xattrs only, no DB

Rejected for the finished design. xattrs are excellent per-object metadata, but they do not provide an efficient global `remote-id → local-object` index or durable transactional operation queue.

### SQLite only, no xattrs

Also rejected. Per-object OneDrive identity and state should remain associated with the local filesystem object for rename behavior, recovery and debuggability.

---

## 24. Final recommended stack

```text
Language:
  daemon/helper: Rust, C++, or another systems language with strong Linux API support
  KDE plugin: C++/Qt/KF6 is the most native option

Filesystem:
  Btrfs first
  ext4 second

Kernel integration:
  fanotify permission/pre-content-capable group
  separate notification mechanism/group for ordinary local changes

Cloud protocol:
  Microsoft Graph
  DriveItem IDs as remote identity
  delta for incremental remote synchronization
  resumable upload sessions for large files

Persistent metadata:
  xattrs for intrinsic per-object metadata
  SQLite for indexes, delta state, durable operation journal, conflicts

Desktop integration:
  D-Bus
  KOverlayIconPlugin
  Dolphin context menu/file-item actions
```

---

## 25. Core invariants

The implementation should be judged against these invariants:

1. **A hydrated file is a normal local file.**
2. **An online-only file reports its true remote logical size.**
3. **No application receives placeholder zeros as if they were real file content.**
4. **No network request is required merely to browse a directory or display file state.**
5. **Remote path is never treated as remote identity; DriveItem ID is.**
6. **Local dirty data is never silently overwritten by remote data.**
7. **Remote data is never silently overwritten when optimistic concurrency detects an independent remote edit.**
8. **A crash during hydration/upload/dehydration leaves a recoverable state.**
9. **Pinned is persistent policy, including for future descendants of a recursively pinned directory.**
10. **The privileged component is minimal and contains no cloud credentials.**

---

## 26. References

- Linux kernel releases: https://kernel.org/
- `fanotify(7)`: https://man7.org/linux/man-pages/man7/fanotify.7.html
- `fanotify_init(2)`: https://man7.org/linux/man-pages/man2/fanotify_init.2.html
- `fanotify_mark(2)`: https://man7.org/linux/man-pages/man2/fanotify_mark.2.html
- Microsoft Graph `driveItem: delta`: https://learn.microsoft.com/en-us/graph/api/driveitem-delta?view=graph-rest-1.0
- Microsoft Graph `driveItem` resource: https://learn.microsoft.com/en-us/graph/api/resources/driveitem?view=graph-rest-1.0
- Microsoft Graph download file content: https://learn.microsoft.com/en-us/graph/api/driveitem-get-content?view=graph-rest-1.0
- Microsoft Graph resumable upload session: https://learn.microsoft.com/en-us/graph/api/driveitem-createuploadsession?view=graph-rest-1.0
- KDE `KOverlayIconPlugin`: https://api.kde.org/koverlayiconplugin.html

---

## 27. Conclusion

For a personal Linux OneDrive client targeting Fedora/KDE, the preferred architecture is:

> **ordinary Btrfs files + sparse placeholders + xattrs + SQLite + a userspace sync daemon + a tiny privileged fanotify helper + native Dolphin integration.**

It preserves the major UX advantage of Windows OneDrive Files On-Demand: the visible OneDrive directory is the real local directory, hydrated files are ordinary files, online-only objects have correct logical sizes, and content is fetched transparently on first access.

The key technical risk is not Microsoft Graph and not KDE integration. It is validating the precise fanotify permission-event behavior and crash/concurrency semantics on the target Fedora kernel/Btrfs stack. That should therefore be the first prototype milestone before the rest of the product is built.
