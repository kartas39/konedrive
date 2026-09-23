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

enum class Emblem {
    None,
    Cloud,
    Syncing,
    Downloaded,
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

Emblem emblemFor(FileState state);

/// The overlay icon names Dolphin draws for `emblem`; empty for `Emblem::None`.
QStringList overlayNames(Emblem emblem);

/// The icons of the two context menu actions.
inline constexpr char DownloadIcon[] = "cloud-download";
inline constexpr char FreeUpSpaceIcon[] = "cloudstatus";

/// `/a/b` for `/a/b/c`, `/` for `/a`, empty for `/` or a relative name.
QString parentDirectory(const QString &path);
/// The last component of `path`, empty if there is none.
QString fileName(const QString &path);
QString joinPath(const QString &dir, const QString &name);

/// Which of `paths` each context menu action applies to.
struct ActionTargets {
    QStringList download;
    QStringList freeUpSpace;
};

/// "Download" fits a file that is online-only, "Free up space" one that is
/// downloaded, and neither fits anything outside a sync root. Reads each
/// file's attributes by path, never opening one; a root is looked up once
/// per directory for the duration of this call only.
ActionTargets actionTargets(const QStringList &paths, const RootMarkReader &hasMark = hasRootMark);

} // namespace konedrive
