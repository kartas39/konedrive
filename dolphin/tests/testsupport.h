// Shared by the Dolphin plugin tests: sync folders built from real files and
// real extended attributes, a stand-in for konedrived on the private bus, and
// an inotify watch that reports every open.

#pragma once

#include <QDBusConnection>
#include <QDBusContext>
#include <QDBusMessage>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QHash>
#include <QObject>
#include <QStringList>
#include <QTemporaryDir>
#include <QTimer>
#include <QUrl>

#include <cerrno>
#include <cstring>

#include <fcntl.h>
#include <sys/inotify.h>
#include <sys/xattr.h>
#include <unistd.h>

namespace testsupport
{

inline QByteArray native(const QString &path)
{
    return QFile::encodeName(path);
}

inline bool setAttribute(const QString &path, const char *name, const QByteArray &value)
{
    if (::lsetxattr(native(path).constData(), name, value.constData(), value.size(), 0) != 0) {
        qWarning("setting %s on %s: %s", name, qPrintable(path), std::strerror(errno));
        return false;
    }
    return true;
}

inline bool removeAttribute(const QString &path, const char *name)
{
    if (::lremovexattr(native(path).constData(), name) != 0) {
        qWarning("removing %s from %s: %s", name, qPrintable(path), std::strerror(errno));
        return false;
    }
    return true;
}

/// By path, as `setfattr` would: this opens nothing.
inline bool setState(const QString &path, const QByteArray &state)
{
    return setAttribute(path, "user.konedrive.state", state);
}

inline bool markRoot(const QString &dir)
{
    return setAttribute(dir, "user.konedrive.root", QByteArrayLiteral("1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d"));
}

/// "Always keep on this device", set the way the daemon's Pin() does: by
/// path, on a file or a folder.
inline bool pin(const QString &path)
{
    return setAttribute(path, "user.konedrive.pin", QByteArrayLiteral("1"));
}

inline bool unpin(const QString &path)
{
    return removeAttribute(path, "user.konedrive.pin");
}

inline QUrl url(const QString &path)
{
    return QUrl::fromLocalFile(path);
}

/// A temporary tree of folders and files, removed with it.
class Tree
{
public:
    bool isValid() const
    {
        return m_dir.isValid();
    }

    QString path(const QString &relative) const
    {
        return m_dir.filePath(relative);
    }

    bool dir(const QString &relative)
    {
        return QDir().mkpath(path(relative));
    }

    /// A sync root: a folder carrying `user.konedrive.root`.
    bool root(const QString &relative)
    {
        return dir(relative) && markRoot(path(relative));
    }

    /// An empty regular file, with `user.konedrive.state` set to `state`
    /// unless it is empty. Creating it opens it -- only ever done before
    /// anything watches for opens.
    bool file(const QString &relative, const QByteArray &state = {})
    {
        const QString full = path(relative);
        if (!QDir().mkpath(QFileInfo(full).path())) {
            return false;
        }
        const int fd = ::open(native(full).constData(), O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
        if (fd < 0) {
            qWarning("creating %s: %s", qPrintable(full), std::strerror(errno));
            return false;
        }
        ::close(fd);
        return state.isEmpty() || setState(full, state);
    }

    bool symlink(const QString &target, const QString &relative)
    {
        return ::symlink(native(path(target)).constData(), native(path(relative)).constData()) == 0;
    }

private:
    QTemporaryDir m_dir;
};

/// Stands in for konedrived's `org.konedrive.Sync1` on the private session
/// bus, on a connection of its own -- so calls to it really cross the bus.
///
/// Pin(as), Unpin(as) and FreeUp(as) each take the whole batch of paths in one call and
/// answer with one aggregate result, not one per path, so there is one
/// `defaultAnswer` for whatever the next call gets -- not one per path, the
/// way the old per-file Hydrate/Dehydrate stand-in needed.
class FakeSync : public QObject, protected QDBusContext
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.konedrive.Sync1")

public:
    struct Answer {
        QString errorName;
        QString message;
        int delayMs = 0;
        /// Pin's `queued`.
        uint queued = 0;
        /// Unpin's `unpinned`.
        uint unpinned = 0;
        /// FreeUp's `files`, `bytes`, `busy`, `skipped_pinned`.
        uint files = 0;
        qulonglong bytes = 0;
        uint busy = 0;
        uint skippedPinned = 0;
    };

    /// How the next call to Pin or FreeUp is answered; succeeds at once
    /// unless set otherwise. A negative delay never answers at all.
    Answer defaultAnswer;
    /// "Pin a.bin,b.bin" or "FreeUp a.bin,b.bin", in the order calls arrived.
    QStringList calls;
    /// Delayed answers sent so far.
    int delayedAnswersSent = 0;

    ~FakeSync() override
    {
        stop();
    }

    static QString connectionName()
    {
        return QStringLiteral("fake-konedrived");
    }

    bool start()
    {
        QDBusConnection bus = QDBusConnection::connectToBus(QDBusConnection::SessionBus, connectionName());
        return bus.isConnected()
            && bus.registerObject(QStringLiteral("/org/konedrive/Daemon"), this, QDBusConnection::ExportAllSlots)
            && bus.registerService(QStringLiteral("org.konedrive.Daemon"));
    }

    /// Leaves the bus as a daemon that stops does: a call still waiting for
    /// an answer is then answered NoReply by the bus.
    void stop()
    {
        if (!m_stopped) {
            m_stopped = true;
            QDBusConnection::disconnectFromBus(connectionName());
        }
    }

public Q_SLOTS:
    void Pin(const QStringList &paths, const QDBusMessage &message)
    {
        answer(QStringLiteral("Pin"), paths, message, {QVariant::fromValue(defaultAnswer.queued)});
    }

    void Unpin(const QStringList &paths, const QDBusMessage &message)
    {
        answer(QStringLiteral("Unpin"), paths, message, {QVariant::fromValue(defaultAnswer.unpinned)});
    }

    void FreeUp(const QStringList &paths, const QDBusMessage &message)
    {
        answer(QStringLiteral("FreeUp"),
               paths,
               message,
               {QVariant::fromValue(defaultAnswer.files), QVariant::fromValue(defaultAnswer.bytes), QVariant::fromValue(defaultAnswer.busy),
                QVariant::fromValue(defaultAnswer.skippedPinned)});
    }

private:
    void answer(const QString &method, const QStringList &paths, const QDBusMessage &message, const QVariantList &results)
    {
        calls.append(method + QLatin1Char(' ') + paths.join(QLatin1Char(',')));
        const Answer a = defaultAnswer;
        message.setDelayedReply(true);
        if (a.delayMs < 0) {
            return;
        }
        const QDBusMessage reply = a.errorName.isEmpty() ? message.createReply(results) : message.createErrorReply(a.errorName, a.message);
        if (a.delayMs == 0) {
            QDBusConnection(connectionName()).send(reply);
            return;
        }
        QTimer::singleShot(a.delayMs, this, [this, reply]() {
            if (!m_stopped) {
                QDBusConnection(connectionName()).send(reply);
                ++delayedAnswersSent;
            }
        });
    }

    bool m_stopped = false;
};

/// Every open(2) and read(2) of anything directly inside the watched
/// directories, the directories themselves included. IN_OPEN and IN_ACCESS
/// need no privilege.
class OpenWatch
{
public:
    explicit OpenWatch(const QStringList &dirs)
        : m_fd(::inotify_init1(IN_NONBLOCK | IN_CLOEXEC))
    {
        for (const QString &dir : dirs) {
            const int wd = ::inotify_add_watch(m_fd, native(dir).constData(), IN_OPEN | IN_ACCESS | IN_ONLYDIR);
            if (wd < 0) {
                qWarning("watching %s: %s", qPrintable(dir), std::strerror(errno));
                m_valid = false;
            }
            m_dirs.insert(wd, dir);
        }
    }

    ~OpenWatch()
    {
        ::close(m_fd);
    }

    bool isValid() const
    {
        return m_fd >= 0 && m_valid;
    }

    int fd() const
    {
        return m_fd;
    }

    /// What was opened or read since the last call, as paths.
    QStringList take()
    {
        QStringList seen;
        alignas(inotify_event) char buffer[16384];
        for (;;) {
            const ssize_t length = ::read(m_fd, buffer, sizeof buffer);
            if (length <= 0) {
                break;
            }
            for (const char *cursor = buffer; cursor < buffer + length;) {
                const auto *event = reinterpret_cast<const inotify_event *>(cursor);
                cursor += sizeof(inotify_event) + event->len;
                const QString dir = m_dirs.value(event->wd);
                const QString what = event->len ? dir + QLatin1Char('/') + QFile::decodeName(event->name) : dir;
                if (event->mask & IN_OPEN) {
                    seen.append(QStringLiteral("opened ") + what);
                }
                if (event->mask & IN_ACCESS) {
                    seen.append(QStringLiteral("read ") + what);
                }
                if (event->mask & IN_Q_OVERFLOW) {
                    seen.append(QStringLiteral("events lost"));
                }
            }
        }
        return seen;
    }

private:
    int m_fd;
    bool m_valid = true;
    QHash<int, QString> m_dirs;
};

} // namespace testsupport
