#include "overlayengine.h"

#include <QFile>
#include <QLoggingCategory>
#include <QSet>
#include <QSocketNotifier>

#include <algorithm>
#include <cerrno>
#include <cstring>

#include <sys/inotify.h>
#include <unistd.h>

Q_LOGGING_CATEGORY(KONEDRIVE_OVERLAY, "konedrive.dolphin.overlay", QtWarningMsg)

namespace konedrive
{

namespace
{
// IN_ATTRIB: a child's attributes (its state) or the directory's own (its
// root mark). IN_DELETE / IN_MOVED_FROM: a name to stop tracking.
// IN_MOVED_TO: a name replaced by a rename. The *_SELF events: this
// directory's path no longer leads to it.
constexpr uint32_t WatchMask = IN_ATTRIB | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO | IN_DELETE_SELF | IN_MOVE_SELF | IN_ONLYDIR | IN_EXCL_UNLINK;

bool isInside(const QString &path, const QString &dir)
{
    return path == dir || path.startsWith(dir.endsWith(QLatin1Char('/')) ? dir : QString(dir + QLatin1Char('/')));
}
} // namespace

OverlayEngine::OverlayEngine(QObject *parent, int directoryLimit, RootMarkReader hasMark)
    : QObject(parent)
    , m_directoryLimit(std::max(directoryLimit, 2))
    , m_hasMark(std::move(hasMark))
{
}

OverlayEngine::~OverlayEngine()
{
    if (m_inotify >= 0) {
        ::close(m_inotify);
    }
}

QStringList OverlayEngine::overlays(const QUrl &url)
{
    if (!url.isLocalFile()) {
        return {};
    }
    const QString path = url.toLocalFile();
    const QString dir = parentDirectory(path);
    const QString name = fileName(path);
    if (dir.isEmpty() || name.isEmpty()) {
        return {};
    }

    Directory *directory = directoryFor(dir);
    // Without a watch there is nothing to keep a cached answer true, so it is
    // worked out afresh each time instead.
    const bool inRoot = directory ? directory->root.has_value() : rootOf(dir, m_hasMark).has_value();
    if (!inRoot) {
        return {};
    }

    const Emblem emblem = emblemFor(readFileState(path));
    if (directory) {
        directory->files.insert(name, emblem);
    }
    return overlayNames(emblem);
}

int OverlayEngine::directoryCount() const
{
    return static_cast<int>(m_directories.size());
}

int OverlayEngine::watchCount() const
{
    return m_pathsByWatch.size();
}

OverlayEngine::Directory *OverlayEngine::directoryFor(QString dir)
{
    if (auto it = m_directories.find(dir); it != m_directories.end()) {
        use(it->second);
        return &it->second;
    }
    if (!ensureInotify()) {
        return nullptr;
    }

    makeRoom();
    // The watch first, the attribute second: a root mark set or removed after
    // the read below is then reported, never missed.
    const int watch = ::inotify_add_watch(m_inotify, QFile::encodeName(dir).constData(), WatchMask);
    if (watch < 0) {
        if (errno == ENOSPC && !m_warnedOutOfWatches) {
            m_warnedOutOfWatches = true;
            qCWarning(KONEDRIVE_OVERLAY) << "out of inotify watches (fs.inotify.max_user_watches): emblems are still drawn, but no longer update on their own";
        }
        return nullptr;
    }

    Directory &directory = m_directories[dir];
    directory.watch = watch;
    m_pathsByWatch[watch].append(dir);
    directory.root = rootOf(dir, m_hasMark);
    use(directory);

    if (directory.root && *directory.root != dir) {
        const QString root = *directory.root;
        // Watching the root is what notices it losing its mark or moving.
        // Its entry is created now, and checked once more after its watch
        // exists, so a mark removed in between is not cached either.
        Directory *rootEntry = directoryFor(root);
        auto self = m_directories.find(dir);
        if (self == m_directories.end()) {
            return nullptr;
        }
        if (!rootEntry || rootEntry->root != root) {
            self->second.root = rootOf(dir, m_hasMark);
        }
        use(self->second);
        return &self->second;
    }
    return &directory;
}

void OverlayEngine::use(Directory &directory)
{
    directory.lastUse = ++m_clock;
    // A root is never less recently used than a directory inside it, so it
    // is evicted after them, not before.
    if (directory.root) {
        if (auto root = m_directories.find(*directory.root); root != m_directories.end() && &root->second != &directory) {
            root->second.lastUse = ++m_clock;
        }
    }
}

bool OverlayEngine::ensureInotify()
{
    if (m_inotify >= 0) {
        return true;
    }
    if (m_inotifyUnavailable) {
        return false;
    }
    m_inotify = ::inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (m_inotify < 0) {
        m_inotifyUnavailable = true;
        qCWarning(KONEDRIVE_OVERLAY) << "cannot use inotify:" << std::strerror(errno) << "- emblems will not update on their own";
        return false;
    }
    m_notifier = new QSocketNotifier(m_inotify, QSocketNotifier::Read, this);
    connect(m_notifier, &QSocketNotifier::activated, this, &OverlayEngine::readEvents);
    return true;
}

void OverlayEngine::makeRoom()
{
    while (static_cast<int>(m_directories.size()) >= m_directoryLimit) {
        auto oldest = std::min_element(m_directories.begin(), m_directories.end(), [](const auto &a, const auto &b) {
            return a.second.lastUse < b.second.lastUse;
        });
        forget(oldest->first);
    }
}

void OverlayEngine::forget(QString dir)
{
    auto it = m_directories.find(dir);
    if (it == m_directories.end()) {
        return;
    }
    const int watch = it->second.watch;
    const bool isRoot = it->second.root == dir;
    m_directories.erase(it);

    if (auto paths = m_pathsByWatch.find(watch); paths != m_pathsByWatch.end()) {
        paths->removeAll(dir);
        if (paths->isEmpty()) {
            m_pathsByWatch.erase(paths);
            ::inotify_rm_watch(m_inotify, watch);
        }
    }

    // An answer naming this root is no longer backed by a watch on it.
    if (isRoot) {
        QStringList dependents;
        for (const auto &[path, directory] : m_directories) {
            if (directory.root == dir) {
                dependents.append(path);
            }
        }
        for (const QString &path : std::as_const(dependents)) {
            forget(path);
        }
    }
}

void OverlayEngine::forgetTree(QString dir)
{
    QStringList doomed;
    for (const auto &[path, directory] : m_directories) {
        if (isInside(path, dir) || directory.root == dir) {
            doomed.append(path);
        }
    }
    for (const QString &path : std::as_const(doomed)) {
        forget(path);
    }
}

void OverlayEngine::resolveAgain(const QStringList &dirs, Changes &changes)
{
    // Everything whose answer could depend on these directories' marks:
    // themselves, what lies below them, and what names them as its root.
    QStringList affected;
    for (const auto &[path, directory] : m_directories) {
        const bool depends = std::any_of(dirs.begin(), dirs.end(), [&](const QString &dir) {
            return isInside(path, dir) || directory.root == dir;
        });
        if (depends) {
            affected.append(path);
        }
    }

    QSet<QString> roots;
    for (const QString &path : std::as_const(affected)) {
        auto it = m_directories.find(path);
        if (it == m_directories.end()) {
            continue;
        }
        Directory &directory = it->second;
        const std::optional<QString> before = directory.root;
        directory.root = rootOf(path, m_hasMark);
        if (directory.root == before) {
            continue;
        }
        if (!directory.root) {
            for (auto file = directory.files.cbegin(); file != directory.files.cend(); ++file) {
                if (file.value() != Emblem::None) {
                    changes.append({QUrl::fromLocalFile(joinPath(path, file.key())), QStringList()});
                }
            }
            directory.files.clear();
            continue;
        }
        if (*directory.root != path) {
            roots.insert(*directory.root);
        }
        const QStringList names = directory.files.keys();
        for (const QString &name : names) {
            recheck(path, name, changes);
        }
    }
    for (const QString &root : std::as_const(roots)) {
        directoryFor(root);
    }
}

void OverlayEngine::recheck(const QString &dir, const QString &name, Changes &changes)
{
    auto it = m_directories.find(dir);
    if (it == m_directories.end() || !it->second.root) {
        return;
    }
    auto file = it->second.files.find(name);
    if (file == it->second.files.end()) {
        return;
    }
    const QString path = joinPath(dir, name);
    const Emblem now = emblemFor(readFileState(path));
    if (now == file.value()) {
        return;
    }
    file.value() = now;
    changes.append({QUrl::fromLocalFile(path), overlayNames(now)});
}

void OverlayEngine::readEvents()
{
    QStringList gone;
    QStringList marksChanged;
    QList<std::pair<QString, QString>> touched;
    bool overflow = false;

    alignas(inotify_event) char buffer[16384];
    for (;;) {
        const ssize_t length = ::read(m_inotify, buffer, sizeof buffer);
        if (length < 0 && errno == EINTR) {
            continue;
        }
        if (length <= 0) {
            break;
        }
        for (const char *cursor = buffer; cursor < buffer + length;) {
            const auto *event = reinterpret_cast<const inotify_event *>(cursor);
            cursor += sizeof(inotify_event) + event->len;

            if (event->mask & IN_Q_OVERFLOW) {
                overflow = true;
                continue;
            }
            const QStringList dirs = m_pathsByWatch.value(event->wd);
            if (event->mask & IN_IGNORED) {
                // The kernel dropped the watch: the directory was deleted or
                // its filesystem unmounted.
                m_pathsByWatch.remove(event->wd);
                gone.append(dirs);
                continue;
            }
            if (event->mask & (IN_DELETE_SELF | IN_MOVE_SELF | IN_UNMOUNT)) {
                gone.append(dirs);
                continue;
            }
            if (event->len == 0 || event->name[0] == '\0') {
                if (event->mask & IN_ATTRIB) {
                    marksChanged.append(dirs);
                }
                continue;
            }
            const QString name = QFile::decodeName(event->name);
            for (const QString &dir : dirs) {
                auto it = m_directories.find(dir);
                if (it == m_directories.end()) {
                    continue;
                }
                if (event->mask & (IN_DELETE | IN_MOVED_FROM)) {
                    it->second.files.remove(name);
                } else if (it->second.files.contains(name)) {
                    touched.append({dir, name});
                }
            }
        }
    }

    Changes changes;
    for (const QString &dir : std::as_const(gone)) {
        forgetTree(dir);
    }
    if (overflow) {
        // Events were lost, so nothing cached can be trusted: every answer
        // is worked out again and every file Dolphin was told about is read
        // again.
        QStringList all;
        for (const auto &[path, directory] : m_directories) {
            all.append(path);
        }
        resolveAgain(all, changes);
        for (const QString &dir : std::as_const(all)) {
            if (auto it = m_directories.find(dir); it != m_directories.end()) {
                const QStringList names = it->second.files.keys();
                for (const QString &name : names) {
                    recheck(dir, name, changes);
                }
            }
        }
    } else if (!marksChanged.isEmpty()) {
        resolveAgain(marksChanged, changes);
    }
    for (const auto &[dir, name] : std::as_const(touched)) {
        recheck(dir, name, changes);
    }

    // Emitted last: a receiver asks for overlays again at once, and that may
    // change the cache this function was walking.
    for (const auto &[url, overlays] : std::as_const(changes)) {
        Q_EMIT overlaysChanged(url, overlays);
    }
}

} // namespace konedrive
