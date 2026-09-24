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

/// Windows-like, following the design: a cloud for online-only, an outline
/// check for a hydrated file nobody asked to keep, a filled check for one
/// that is effectively pinned, and the syncing glyph both for a file moving
/// between states and for one pinned but not yet downloaded.
enum class Emblem {
    None,
    Cloud,
    Syncing,
    CheckOutline,
    CheckFilled,
};

/// `path`'s state, from lstat(2) and lgetxattr(2). Never opens `path`, and
/// never follows a symbolic link: a link does not borrow its target's state.
FileState readFileState(const QString &path);

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

/// The nearest ancestor of `path`, up to and including `root`, that carries
/// the pin -- physical ancestors (physicalDirectory, the same resolution
/// `root` itself was found with), never `path` itself. `std::nullopt` if
/// none of them does. Independent of whether `path` also carries its own
/// pin: the daemon still refuses Unpin/FreeUp for a path pinned by an
/// ancestor even when it is explicitly pinned too, since it would stay
/// pinned by that ancestor either way (pinning.md §5) -- so this is what
/// the menu's "pinned above" checks need, not `pinnedBy`.
std::optional<QString> pinnedAbove(const QString &path, const QString &root, const PinMarkReader &hasPin = hasPinMark);

/// The nearest item at or above `path`, up to and including `root`, that
/// carries the pin: `path` itself first, then `pinnedAbove`. `std::nullopt`
/// if none of them does.
std::optional<QString> pinnedBy(const QString &path, const QString &root, const PinMarkReader &hasPin = hasPinMark);

/// Whether `path` is pinned, itself or through an ancestor (pinnedBy).
bool isEffectivelyPinned(const QString &path, const QString &root, const PinMarkReader &hasPin = hasPinMark);

/// `state`'s emblem, following `pinned` (whether the item is effectively
/// pinned) for the two hydrated cases and for an online-only file the sweep
/// has queued.
Emblem emblemFor(FileState state, bool pinned);

/// Whether `path`, by lstat(2) without following it, is a directory.
bool isDirectory(const QString &path);

/// The emblem for an item that may be a directory: a directory shows the
/// filled check when it is effectively pinned, and nothing otherwise (it has
/// no `FileState` of its own); anything else follows `emblemFor`.
Emblem emblemForItem(FileState state, bool isDir, bool pinned);

/// The overlay icon names Dolphin draws for `emblem`; empty for `Emblem::None`.
QStringList overlayNames(Emblem emblem);

/// The icon of "Always keep on this device"; "Free up space" keeps the cloud
/// icon it always had.
inline constexpr char AlwaysKeepIcon[] = "window-pin";
inline constexpr char FreeUpSpaceIcon[] = "cloudstatus";

/// `/a/b` for `/a/b/c`, `/` for `/a`, empty for `/` or a relative name.
QString parentDirectory(const QString &path);
/// The last component of `path`, empty if there is none.
QString fileName(const QString &path);
QString joinPath(const QString &dir, const QString &name);

/// Whether `path`, by lstat(2) without following it, is a regular file or a
/// directory -- what "Always keep on this device" and "Free up space" can
/// apply to. A symbolic link is neither: it never borrows its target's pin.
bool isFileOrDirectory(const QString &path);

/// What the context menu offers for a selection.
struct MenuState {
    /// The selected paths that lie inside a sync root -- files or folders,
    /// never a symbolic link, and never unmanaged, unrecognised or reserved
    /// (`.konedrive-*`) -- in the order they were given, and what Pin(),
    /// Unpin() or FreeUp() is called with: one bad item must not make the
    /// daemon refuse the whole batch.
    QStringList inRoot;
    /// "Always keep on this device": offered whenever `inRoot` is not empty.
    bool showAlwaysKeep = false;
    /// Checked when every item of `inRoot` is effectively pinned. Unchecking
    /// it calls Unpin(), not FreeUp() (Windows-like: unpinning never frees
    /// space on its own).
    bool alwaysKeepChecked = false;
    /// While unchecked, toggling it (Pin()) is always safe -- a path already
    /// covered by an ancestor is left as it is -- so this only matters while
    /// checked: disabled when *any* item of `inRoot` is pinned by an
    /// ancestor, since Unpin() refuses the whole call if any path it is
    /// given is -- even one that is also explicitly pinned itself, which
    /// would stay pinned by that ancestor either way (pinning.md §5).
    bool alwaysKeepEnabled = true;
    /// "Free up space": offered when anything in `inRoot` is a folder,
    /// hydrated, or explicitly pinned (Windows-like: it works on any folder
    /// in the root, not only a downloaded or pinned one).
    bool showFreeUp = false;
    /// Disabled when anything in `inRoot` is pinned by an ancestor (again
    /// regardless of its own pin): FreeUp() refuses the whole call if any
    /// path it is given is.
    bool freeUpEnabled = true;
    /// The ancestor folder's name, set whenever an item of `inRoot` is
    /// pinned by an ancestor -- named in "Always keep"'s tooltip when it is
    /// disabled, and in "Free up space"'s when it is.
    QString blockingFolder;
};

/// Classifies `paths` for the context menu. Reads each item's attributes by
/// path, never opening one; a root is looked up once per directory for the
/// duration of this call only.
MenuState menuState(const QStringList &paths, const RootMarkReader &hasRoot = hasRootMark, const PinMarkReader &hasPin = hasPinMark);

} // namespace konedrive
