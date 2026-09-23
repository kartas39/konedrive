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

bool hasRootMark(const QString &dir)
{
    const QByteArray native = QFile::encodeName(dir);
    return ::lgetxattr(native.constData(), RootAttribute, nullptr, 0) >= 0;
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

Emblem emblemFor(FileState state)
{
    switch (state) {
    case FileState::OnlineOnly:
        return Emblem::Cloud;
    case FileState::Hydrating:
    case FileState::Dehydrating:
        return Emblem::Syncing;
    case FileState::Hydrated:
        return Emblem::Downloaded;
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
    // status/*/cloudstatus.svg, status/*/state-sync.svg, emblems/*/emblem-checked.svg.
    switch (emblem) {
    case Emblem::Cloud:
        return {QStringLiteral("cloudstatus")};
    case Emblem::Syncing:
        return {QStringLiteral("state-sync")};
    case Emblem::Downloaded:
        return {QStringLiteral("emblem-checked")};
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

ActionTargets actionTargets(const QStringList &paths, const RootMarkReader &hasMark)
{
    ActionTargets targets;
    QHash<QString, bool> inRoot;
    for (const QString &path : paths) {
        const QString dir = parentDirectory(path);
        if (dir.isEmpty()) {
            continue;
        }
        auto known = inRoot.constFind(dir);
        if (known == inRoot.constEnd()) {
            known = inRoot.insert(dir, rootOf(dir, hasMark).has_value());
        }
        if (!known.value()) {
            continue;
        }
        switch (readFileState(path)) {
        case FileState::OnlineOnly:
            targets.download.append(path);
            break;
        case FileState::Hydrated:
            targets.freeUpSpace.append(path);
            break;
        default:
            break;
        }
    }
    return targets;
}

} // namespace konedrive
