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

std::optional<QString> pinnedAbove(const QString &path, const QString &root, const PinMarkReader &hasPin)
{
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

std::optional<QString> pinnedBy(const QString &path, const QString &root, const PinMarkReader &hasPin)
{
    // The item itself, as Dolphin spelled it: a placeholder is never a
    // symbolic link, so this is also its physical path.
    if (hasPin(path)) {
        return path;
    }
    return pinnedAbove(path, root, hasPin);
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

bool isFileOrDirectory(const QString &path)
{
    const QByteArray native = QFile::encodeName(path);
    struct stat info {
    };
    return ::lstat(native.constData(), &info) == 0 && (S_ISREG(info.st_mode) || S_ISDIR(info.st_mode));
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

namespace
{
/// Names konedrive keeps for itself (crates/konedrived/src/sync/root.rs);
/// never offered, so one of them can't make Pin/Unpin/FreeUp refuse the
/// whole batch it is part of.
bool isReservedName(const QString &path)
{
    return fileName(path).startsWith(QLatin1String(".konedrive-"));
}
} // namespace

MenuState menuState(const QStringList &paths, const RootMarkReader &hasRoot, const PinMarkReader &hasPin)
{
    MenuState result;
    QHash<QString, std::optional<QString>> rootByDir;

    bool anyConsidered = false;
    bool allEffectivelyPinned = true;
    bool anyPinnedAbove = false;
    bool anyFolder = false;
    bool anyHydratedOrExplicit = false;

    for (const QString &path : paths) {
        const QString dir = parentDirectory(path);
        if (dir.isEmpty() || !isFileOrDirectory(path) || isReservedName(path)) {
            continue;
        }
        auto known = rootByDir.find(dir);
        if (known == rootByDir.end()) {
            known = rootByDir.insert(dir, rootOf(dir, hasRoot));
        }
        if (!known.value().has_value()) {
            continue;
        }
        const QString &root = *known.value();
        const bool isDir = isDirectory(path);
        const FileState state = isDir ? FileState::NotAFile : readFileState(path);
        // Unmanaged and unrecognised files are not offered anything either:
        // Pin/Unpin/FreeUp would have nothing to do with them, and sending
        // one along would only risk refusing the rest of the batch with it.
        if (!isDir && (state == FileState::Unmanaged || state == FileState::Unrecognised)) {
            continue;
        }

        anyConsidered = true;
        result.inRoot.append(path);
        anyFolder = anyFolder || isDir;

        // "Pinned above" is independent of the item's own pin: the daemon
        // still refuses Unpin/FreeUp for it even when it is also explicitly
        // pinned, since it would stay pinned by that ancestor either way
        // (pinning.md §5) -- pinnedBy's short-circuit on the item itself
        // would hide that.
        const std::optional<QString> above = pinnedAbove(path, root, hasPin);
        const bool explicitPin = hasPin(path);
        const bool effectivePinned = explicitPin || above.has_value();
        const bool hydrated = state == FileState::Hydrated;

        allEffectivelyPinned = allEffectivelyPinned && effectivePinned;
        if (above) {
            anyPinnedAbove = true;
            if (result.blockingFolder.isEmpty()) {
                result.blockingFolder = fileName(*above);
            }
        }
        if (hydrated || explicitPin) {
            anyHydratedOrExplicit = true;
        }
    }

    if (!anyConsidered) {
        return result;
    }

    result.showAlwaysKeep = true;
    result.alwaysKeepChecked = allEffectivelyPinned;
    // Unchecked, toggling it (Pin()) is always safe; checked, unchecking it
    // (Unpin()) refuses the whole call if anything is pinned above.
    result.alwaysKeepEnabled = !allEffectivelyPinned || !anyPinnedAbove;
    // Windows-like: any folder in the root offers "Free up space", not only
    // one that is downloaded or pinned.
    result.showFreeUp = anyHydratedOrExplicit || anyFolder;
    result.freeUpEnabled = !anyPinnedAbove;
    return result;
}

} // namespace konedrive
