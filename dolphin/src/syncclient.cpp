#include "syncclient.h"

#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QTimer>

#include <memory>

namespace konedrive
{

const QString SyncClient::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString SyncClient::ObjectPath = QStringLiteral("/org/konedrive/Accounts");
const QString SyncClient::InterfaceName = QStringLiteral("org.konedrive.Files1");

namespace
{
struct Request {
    Operation operation;
    qsizetype unanswered = 0;
    QList<Failure> unreported;
    QTimer *timer = nullptr;
};
} // namespace

SyncClient::SyncClient(const QDBusConnection &bus, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
{
}

void SyncClient::start(Operation operation, const QStringList &paths)
{
    if (paths.isEmpty()) {
        return;
    }

    QStringList toSend;
    QSet<QString> sending;
    QList<Failure> notSent;
    for (const QString &path : paths) {
        if (m_waiting.contains(path) || sending.contains(path)) {
            notSent.append({path, QString::fromLatin1(AlreadyWaitingError), QString()});
        } else if (m_waiting.size() + toSend.size() >= MaxCallsInFlight) {
            notSent.append({path, QString::fromLatin1(TooManyWaitingError), QString()});
        } else {
            toSend.append(path);
            sending.insert(path);
        }
    }

    auto request = std::make_shared<Request>();
    request->operation = operation;
    request->unanswered = toSend.size();
    request->unreported = notSent;
    request->timer = new QTimer(this);
    request->timer->setSingleShot(true);
    request->timer->setInterval(ReportDelayMs);

    const auto report = [this, request]() {
        request->timer->stop();
        if (request->unreported.isEmpty()) {
            return;
        }
        const QString message = failureSummary(request->operation, request->unreported);
        request->unreported.clear();
        Q_EMIT failed(message);
    };
    connect(request->timer, &QTimer::timeout, this, report);

    if (toSend.isEmpty()) {
        report();
        request->timer->deleteLater();
        return;
    }
    if (!notSent.isEmpty()) {
        request->timer->start();
    }

    // One call for the whole batch: Pin(as), Unpin(as) and FreeUp(as) each
    // answer with one aggregate result, not one per path.
    QString method;
    switch (operation) {
    case Operation::AlwaysKeep:
        method = QStringLiteral("Pin");
        break;
    case Operation::Unpin:
        method = QStringLiteral("Unpin");
        break;
    case Operation::FreeUpSpace:
        method = QStringLiteral("FreeUp");
        break;
    }
    QDBusMessage call = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, method);
    call << toSend;
    for (const QString &path : std::as_const(toSend)) {
        m_waiting.insert(path);
    }
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(call, CallTimeout), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, request, report, toSend, operation](QDBusPendingCallWatcher *finished) {
        finished->deleteLater();
        for (const QString &path : toSend) {
            m_waiting.remove(path);
        }
        const QDBusPendingReply<> reply = *finished;
        request->unanswered -= toSend.size();
        if (reply.isError()) {
            for (const QString &path : toSend) {
                request->unreported.append({path, reply.error().name(), reply.error().message()});
            }
            // Reported soon -- but not before the not-sent refusals above,
            // which is why the timer is only started if it is not already.
            if (!request->timer->isActive()) {
                request->timer->start();
            }
        } else if (operation == Operation::FreeUpSpace) {
            // FreeUp's `busy` (files, bytes, busy, skipped_pinned) also
            // folds in files changed here and not uploaded (review #6).
            const QDBusPendingReply<uint, qulonglong, uint, uint> freeUpReply = *finished;
            const uint busy = freeUpReply.argumentAt<2>();
            if (busy > 0) {
                Q_EMIT freeUpKeptBusy(busy);
            }
        }
        if (request->unanswered == 0) {
            report();
            request->timer->deleteLater();
        }
    });
}

} // namespace konedrive
