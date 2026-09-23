// Asks konedrived to download or free up files, without ever waiting for it.
//
// Every call is asynchronous and made with no reply timeout: a download can
// take far longer than D-Bus's default 25 seconds, and a call that timed out
// would report a failure while the download went on. A call to a daemon that
// is not running is answered by the bus at once; one that stops while a call
// waits is answered NoReply.
//
// The daemon is started on demand if it can be (it is D-Bus activatable):
// choosing "Download" is an explicit request for it.
//
// A daemon that never answers must not cost Dolphin without bound: each
// waiting call holds a few KB, and on a dbus-daemon bus the calls count
// against the pending-reply budget of Dolphin's own connection. So a file
// that is still waiting is not asked for again, and no more than
// MaxCallsInFlight calls wait at once; the files left out are named in the
// message.

#pragma once

#include "refusaltext.h"

#include <QDBusConnection>
#include <QObject>
#include <QSet>
#include <QStringList>

#include <limits>

namespace konedrive
{

class SyncClient : public QObject
{
    Q_OBJECT

public:
    static const QString ServiceName;
    static const QString ObjectPath;
    static const QString InterfaceName;
    /// DBUS_TIMEOUT_INFINITE.
    static constexpr int CallTimeout = std::numeric_limits<int>::max();
    /// How long failures of one request are gathered into one message
    /// before it is shown, unless every file has been answered sooner.
    static constexpr int ReportDelayMs = 300;
    /// Calls waiting for the daemon at once, per Dolphin window.
    static constexpr int MaxCallsInFlight = 1000;

    explicit SyncClient(const QDBusConnection &bus, QObject *parent = nullptr);

    /// One call per file, all sent at once; the daemon schedules them.
    /// Files already waiting for an answer, and files past the cap, are not
    /// sent; they are reported along with the refusals.
    void start(Operation operation, const QStringList &paths);

Q_SIGNALS:
    /// A message for the user about files the daemon did not do what was
    /// asked for. Nothing is emitted for files it did.
    void failed(const QString &message);

private:
    QDBusConnection m_bus;
    /// Files whose call has been sent and not answered yet.
    QSet<QString> m_waiting;
};

} // namespace konedrive
