#include "filestate.h"

#include <QFile>
#include <QHash>

#include <cerrno>
#include <climits>
#include <string_view>

#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

namespace konedrive
{

FileState readFileState(const QString &path)
{
    const QByteArray native = QFile::encodeName(path);
    struct stat info {
    };
    if (::lstat(native.constData(), &info) != 0 || !S_ISREG(info.st_mode)) {
        return FileState::NotAFile;
    }

    // The longest value is "online-only" / "dehydrating"; anything that does
    // not fit is not a state this plugin knows.
    char value[32];
    const ssize_t size = ::lgetxattr(native.constData(), StateAttribute, value, sizeof value);
    if (size < 0) {
        // ENODATA: no attribute. ENOTSUP: a filesystem without extended
        // attributes, where nothing can be ours either.
        return errno == ERANGE ? FileState::Unrecognised : FileState::Unmanaged;
    }

    const std::string_view state(value, static_cast<size_t>(size));
    if (state == "online-only") {
        return FileState::OnlineOnly;
    }
    if (state == "hydrating") {
        return FileState::Hydrating;
    }
    if (state == "hydrated") {
        return FileState::Hydrated;
    }
    if (state == "dehydrating") {
        return FileState::Dehydrating;
    }
    return FileState::Unrecognised;
}

UploadState readUploadState(const QString &path)
{
    const QByteArray native = QFile::encodeName(path);
    // The longest value is "uploading"; anything that does not fit is not
    // one this plugin knows.
    char value[16];
    const ssize_t size = ::lgetxattr(native.constData(), SyncAttribute, value, sizeof value);
    if (size < 0) {
        return UploadState::None;
    }
    const std::string_view state(value, static_cast<size_t>(size));
    if (state == "pending") {
        return UploadState::Pending;
    }
    if (state == "uploading") {
        return UploadState::Uploading;
    }
    if (state == "blocked") {
        return UploadState::Blocked;
    }
    return UploadState::None;
}

bool hasRootMark(const QString &dir)
{
    const QByteArray native = QFile::encodeName(dir);
    return ::lgetxattr(native.constData(), RootAttribute, nullptr, 0) >= 0;
}

bool hasPinMark(const QString &path)
{
    const QByteArray native = QFile::encodeName(path);
    return ::lgetxattr(native.constData(), PinAttribute, nullptr, 0) >= 0;
}

std::optional<QString> findRoot(const QString &dir, const RootMarkReader &hasMark)
{
    for (QString current = dir; !current.isEmpty(); current = parentDirectory(current)) {
        if (hasMark(current)) {
            return current;
        }
    }
    return std::nullopt;
}

QString physicalDirectory(const QString &dir)
{
    constexpr int MaxLinks = 40; // Linux's own limit when it resolves a path
    if (!dir.startsWith(QLatin1Char('/'))) {
        return {};
    }
    QStringList resolved; // physical components, none of them a link
    QStringList pending = dir.split(QLatin1Char('/'), Qt::SkipEmptyParts);
    int links = 0;
    while (!pending.isEmpty()) {
        const QString part = pending.takeFirst();
        if (part == QLatin1String(".")) {
            continue;
        }
        if (part == QLatin1String("..")) {
            if (!resolved.isEmpty()) {
                resolved.removeLast();
            }
            continue;
        }
        const QByteArray candidate = QFile::encodeName(QLatin1Char('/') + (resolved + QStringList{part}).join(QLatin1Char('/')));
        struct stat info {
        };
        if (::lstat(candidate.constData(), &info) != 0) {
            return {};
        }
        if (!S_ISLNK(info.st_mode)) {
            resolved.append(part);
            continue;
        }
        if (++links > MaxLinks) {
            return {};
        }
        char target[PATH_MAX];
        const ssize_t length = ::readlink(candidate.constData(), target, sizeof target);
        if (length <= 0 || length == static_cast<ssize_t>(sizeof target)) {
            return {};
        }
        const QString link = QFile::decodeName(QByteArray(target, length));
        if (link.startsWith(QLatin1Char('/'))) {
            resolved.clear();
        }
        pending = link.split(QLatin1Char('/'), Qt::SkipEmptyParts) + pending;
    }
    return QLatin1Char('/') + resolved.join(QLatin1Char('/'));
}

std::optional<QString> rootOf(const QString &dir, const RootMarkReader &hasMark)
{
    const QString physical = physicalDirectory(dir);
    if (physical.isEmpty()) {
        return std::nullopt;
    }
    return findRoot(physical, hasMark);
}

std::optional<QString> pinnedBy(const QString &path, const QString &root, const PinMarkReader &hasPin)
{
    // The item itself, as Dolphin spelled it: a placeholder is never a
    // symbolic link, so this is also its physical path.
    if (hasPin(path)) {
        return path;
    }
    // Physical from here on -- the same resolution `root` itself was found
    // with (rootOf), so a directory reached through a symbolic link is
    // compared correctly against it.
    QString dir = physicalDirectory(parentDirectory(path));
    while (!dir.isEmpty()) {
        if (hasPin(dir)) {
            return dir;
        }
        if (dir == root) {
            break;
        }
        dir = parentDirectory(dir);
    }
    return std::nullopt;
}

bool isEffectivelyPinned(const QString &path, const QString &root, const PinMarkReader &hasPin)
{
    return pinnedBy(path, root, hasPin).has_value();
}

Emblem emblemFor(FileState state, bool pinned)
{
    switch (state) {
    case FileState::OnlineOnly:
        // The sweep queues a pinned online-only file for hydration; until
        // that finishes it looks exactly like a file already downloading.
        return pinned ? Emblem::Syncing : Emblem::Cloud;
    case FileState::Hydrating:
    case FileState::Dehydrating:
        return Emblem::Syncing;
    case FileState::Hydrated:
        return pinned ? Emblem::CheckFilled : Emblem::CheckOutline;
    case FileState::Unmanaged:
    case FileState::Unrecognised:
    case FileState::NotAFile:
        break;
    }
    return Emblem::None;
}

QStringList overlayNames(Emblem emblem)
{
    // Breeze names, checked against /usr/share/icons/breeze{,-dark}:
    //   - status/*/cloudstatus.svg (online-only);
    //   - status/*/state-sync.svg (hydrating, dehydrating, or pinned and
    //     still online-only -- the sweep is filling it);
    //   - actions/*/dialog-ok.svg, a bare check with no fill behind it, for
    //     a hydrated file nobody asked to keep (the OUTLINE case). Not
    //     actions/*/checkmark.svg: emblems/*/checkmark.svg is a symlink to
    //     emblem-checked.svg, so that name is ambiguous between the two
    //     directories and could resolve to the filled icon instead;
    //     dialog-ok.svg is the same bare glyph under a name that exists
    //     nowhere else in the theme;
    //   - emblems/*/emblem-checked.svg, the same check filled solid, for one
    //     that is effectively pinned (the FILLED case) -- this is the icon
    //     "hydrated" alone used before pinning existed;
    //   - status/*/state-error.svg, for an item whose upload is blocked
    //     (state-sync, above, serves one waiting to be uploaded too).
    switch (emblem) {
    case Emblem::Cloud:
        return {QStringLiteral("cloudstatus")};
    case Emblem::Syncing:
        return {QStringLiteral("state-sync")};
    case Emblem::CheckOutline:
        return {QStringLiteral("dialog-ok")};
    case Emblem::CheckFilled:
        return {QStringLiteral("emblem-checked")};
    case Emblem::Error:
        return {QStringLiteral("state-error")};
    case Emblem::None:
        break;
    }
    return {};
}

namespace
{
QString withoutTrailingSlashes(const QString &path)
{
    QString result = path;
    while (result.size() > 1 && result.endsWith(QLatin1Char('/'))) {
        result.chop(1);
    }
    return result;
}
} // namespace

QString parentDirectory(const QString &path)
{
    const QString trimmed = withoutTrailingSlashes(path);
    const qsizetype slash = trimmed.lastIndexOf(QLatin1Char('/'));
    if (slash < 0 || trimmed == QLatin1String("/")) {
        return {};
    }
    if (slash == 0) {
        return QStringLiteral("/");
    }
    return trimmed.left(slash);
}

QString fileName(const QString &path)
{
    const QString trimmed = withoutTrailingSlashes(path);
    if (trimmed == QLatin1String("/")) {
        return {};
    }
    return trimmed.mid(trimmed.lastIndexOf(QLatin1Char('/')) + 1);
}

QString joinPath(const QString &dir, const QString &name)
{
    return dir.endsWith(QLatin1Char('/')) ? QString(dir + name) : QString(dir + QLatin1Char('/') + name);
}

bool isDirectory(const QString &path)
{
    const QByteArray native = QFile::encodeName(path);
    struct stat info {
    };
    return ::lstat(native.constData(), &info) == 0 && S_ISDIR(info.st_mode);
}

Emblem emblemForItem(FileState state, bool isDir, bool pinned, UploadState upload)
{
    switch (upload) {
    case UploadState::Pending:
    case UploadState::Uploading:
        return Emblem::Syncing;
    case UploadState::Blocked:
        return Emblem::Error;
    case UploadState::None:
        break;
    }
    if (isDir) {
        return pinned ? Emblem::CheckFilled : Emblem::None;
    }
    return emblemFor(state, pinned);
}

Emblem itemEmblem(const QString &path, const QString &root, const PinMarkReader &hasPin)
{
    // An item with changes waiting to be uploaded needs nothing else read.
    const UploadState upload = readUploadState(path);
    if (upload != UploadState::None) {
        return emblemForItem(FileState::NotAFile, false, false, upload);
    }
    return emblemForItem(readFileState(path), isDirectory(path), isEffectivelyPinned(path, root, hasPin));
}

bool anyInSyncFolder(const QStringList &paths, const RootMarkReader &hasRoot)
{
    // A root is looked up once per directory, for the duration of this call
    // only: a selection is usually many items of one folder.
    QHash<QString, bool> known;
    for (const QString &path : paths) {
        // An account's folder itself counts: it is asked about too.
        const QString dir = isDirectory(path) ? path : parentDirectory(path);
        if (dir.isEmpty()) {
            continue;
        }
        auto found = known.find(dir);
        if (found == known.end()) {
            found = known.insert(dir, rootOf(dir, hasRoot).has_value());
        }
        if (found.value()) {
            return true;
        }
    }
    return false;
}

} // namespace konedrive
