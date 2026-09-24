// Asks konedrived to keep files on this device or free up their space,
// without ever waiting for it.
//
// Pin, Unpin and FreeUp each take the whole selection in one D-Bus call
// (Sync1's `Pin(as) -> u`, `Unpin(as) -> u` and `FreeUp(as) -> (u,t,u,u)`),
// unlike the old per-file Hydrate/Dehydrate this replaced: one call, one
// aggregate answer. The call
// is asynchronous and made with no reply timeout: freeing up a big folder can
// take far longer than D-Bus's default 25 seconds, and a call that timed out
// would report a failure while the work went on. A call to a daemon that is
// not running is answered by the bus at once; one that stops while a call
// waits is answered NoReply.
//
// The daemon is started on demand if it can be (it is D-Bus activatable):
// choosing "Always keep on this device" is an explicit request for it.
//
// A daemon that never answers must not cost Dolphin without bound: each
// waiting call holds a few KB, and on a dbus-daemon bus the calls count
// against the pending-reply budget of Dolphin's own connection. So a path
// that is still waiting (in an earlier call not yet answered) is not sent
// again, and no more than MaxCallsInFlight paths wait at once; the paths left
// out are named in the message. Paths not already waiting and under the cap
// are still sent together, in the one call this operation makes.

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

    /// One Pin(paths) or FreeUp(paths) call for every path not already
    /// waiting and not past the cap; those are reported along with whatever
    /// refusal the call itself comes back with.
    void start(Operation operation, const QStringList &paths);

Q_SIGNALS:
    /// A message for the user about files the daemon did not do what was
    /// asked for. Nothing is emitted for files it did.
    void failed(const QString &message);
    /// FreeUp succeeded, and `busy` of the paths it was given were in use or
    /// changed here (not uploaded yet) and so were kept, not freed --
    /// FreeUp's own `busy` count, which folds in both. Not emitted when it
    /// is 0.
    void freeUpKeptBusy(uint busy);

private:
    QDBusConnection m_bus;
    /// Files whose call has been sent and not answered yet.
    QSet<QString> m_waiting;
};

} // namespace konedrive
