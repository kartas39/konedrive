// What a file in the sync folder is, read from its extended attributes alone.
//
// Everything here answers by path, with lstat(2), lgetxattr(2) and getxattr(2).
// None of them opens anything: in this project opening a placeholder
// downloads it, so a plugin that opened files to decide what to draw would
// download every file Dolphin shows.

#pragma once

#include <QString>
#include <QStringList>

#include <functional>
#include <optional>

namespace konedrive
{

/// The attribute names, as `crates/konedrive-fs/src/placeholder.rs` defines them.
inline constexpr char StateAttribute[] = "user.konedrive.state";
inline constexpr char RootAttribute[] = "user.konedrive.root";
/// "Always keep on this device": set on the pinned file or folder itself.
/// An item is *effectively* pinned when it or any ancestor up to the sync
/// root carries this (isEffectivelyPinned / pinnedBy below).
inline constexpr char PinAttribute[] = "user.konedrive.pin";
/// Written by the daemon on an item with changes waiting to be uploaded
/// (an outbox row, `docs/design/writes.md` §5.4): "pending", "uploading" or "blocked";
/// removed once the upload is committed.
inline constexpr char SyncAttribute[] = "user.konedrive.sync";

enum class FileState {
    /// A regular file with no `user.konedrive.state`: not a OneDrive file.
    Unmanaged,
    OnlineOnly,
    Hydrating,
    Hydrated,
    Dehydrating,
    /// The attribute holds a value this plugin does not know.
    Unrecognised,
    /// A directory, a symbolic link, anything else that is not a regular
    /// file, or nothing at all.
    NotAFile,
};

/// What `user.konedrive.sync` says.
enum class UploadState {
    /// No attribute, or a value this plugin does not know: the item's other
    /// attributes decide its emblem.
    None,
    Pending,
    Uploading,
    /// The upload cannot go on until something changes (a name OneDrive
    /// refuses, a full OneDrive, ...).
    Blocked,
};

/// Windows-like, following the design: a cloud for online-only, an outline
/// check for a hydrated file nobody asked to keep, a filled check for one
/// that is effectively pinned, and the syncing glyph for a file moving
/// between states, for one pinned but not yet downloaded, and for one
/// waiting to be uploaded; an error sign for one whose upload is blocked.
enum class Emblem {
    None,
    Cloud,
    Syncing,
    CheckOutline,
    CheckFilled,
    Error,
};

/// `path`'s state, from lstat(2) and lgetxattr(2). Never opens `path`, and
/// never follows a symbolic link: a link does not borrow its target's state.
FileState readFileState(const QString &path);

/// `path`'s upload state, from lgetxattr(2) alone: never opens `path`, and
/// never follows a symbolic link. Read on files and folders alike.
UploadState readUploadState(const QString &path);

/// Whether `dir` itself carries `user.konedrive.root`. Does not follow a
/// symbolic link: links are resolved beforehand (physicalDirectory), so a
/// link to the root counts through that one mechanism only.
bool hasRootMark(const QString &dir);

using RootMarkReader = std::function<bool(const QString &dir)>;

/// The nearest directory at or above `dir` that carries the root mark,
/// walking `dir` as spelled: give it a physical path (below).
std::optional<QString> findRoot(const QString &dir, const RootMarkReader &hasMark = hasRootMark);

/// `dir` with every symbolic link and `.`/`..` in it resolved, as
/// realpath(3) would -- but by lstat(2) and readlink(2) on each component
/// alone, never opening anything. Empty if it cannot be resolved: a
/// component is missing, or there are more than 40 links.
QString physicalDirectory(const QString &dir);

/// The sync root the directory `dir` physically lies in. A symbolic link to
/// the root counts as inside it; a link inside the root that points out of
/// it does not.
std::optional<QString> rootOf(const QString &dir, const RootMarkReader &hasMark = hasRootMark);

/// Whether `path` itself -- a file or a folder, never following a symbolic
/// link -- carries `user.konedrive.pin`.
bool hasPinMark(const QString &path);

using PinMarkReader = std::function<bool(const QString &path)>;

/// The nearest item at or above `path`, up to and including `root`, that
/// carries the pin: `path` itself first, then its physical ancestors
/// (physicalDirectory, the same resolution `root` itself was found with).
/// `std::nullopt` if none of them does.
std::optional<QString> pinnedBy(const QString &path, const QString &root, const PinMarkReader &hasPin = hasPinMark);

/// Whether `path` is pinned, itself or through an ancestor (pinnedBy).
bool isEffectivelyPinned(const QString &path, const QString &root, const PinMarkReader &hasPin = hasPinMark);

/// `state`'s emblem, following `pinned` (whether the item is effectively
/// pinned) for the two hydrated cases and for an online-only file the sweep
/// has queued.
Emblem emblemFor(FileState state, bool pinned);

/// Whether `path`, by lstat(2) without following it, is a directory.
bool isDirectory(const QString &path);

/// The emblem for an item that may be a directory. An upload waiting or
/// under way shows the syncing glyph and a blocked one the error sign,
/// whatever else holds. Otherwise a directory shows the filled check when it
/// is effectively pinned, and nothing else (it has no `FileState` of its
/// own); anything else follows `emblemFor`.
Emblem emblemForItem(FileState state, bool isDir, bool pinned, UploadState upload = UploadState::None);

/// `path`'s emblem inside `root`, from its attributes alone: its upload
/// state first, and only without one its state and pin.
Emblem itemEmblem(const QString &path, const QString &root, const PinMarkReader &hasPin = hasPinMark);

/// The overlay icon names Dolphin draws for `emblem`; empty for `Emblem::None`.
QStringList overlayNames(Emblem emblem);

/// The icon of "Always keep on this device"; "Free up space" keeps the cloud
/// icon it always had.
inline constexpr char AlwaysKeepIcon[] = "window-pin";
inline constexpr char FreeUpSpaceIcon[] = "cloudstatus";
inline constexpr char OpenOnlineIcon[] = "internet-services";

/// `/a/b` for `/a/b/c`, `/` for `/a`, empty for `/` or a relative name.
QString parentDirectory(const QString &path);
/// The last component of `path`, empty if there is none.
QString fileName(const QString &path);
QString joinPath(const QString &dir, const QString &name);

/// Whether any of `paths` lies in a sync folder, or is one, by the marks
/// alone: the nearest directory at or above it carries the root mark
/// (rootOf). The context menu asks the daemon only then, so a right click
/// anywhere else costs no call; everything else about the menu is the
/// daemon's answer (SyncClient::menu).
bool anyInSyncFolder(const QStringList &paths, const RootMarkReader &hasRoot = hasRootMark);

} // namespace konedrive
