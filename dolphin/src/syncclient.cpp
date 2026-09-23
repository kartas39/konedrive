#include "syncclient.h"

#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QTimer>

#include <memory>

namespace konedrive
{

const QString SyncClient::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString SyncClient::ObjectPath = QStringLiteral("/org/konedrive/Daemon");
const QString SyncClient::InterfaceName = QStringLiteral("org.konedrive.Sync1");

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

    const QString method = operation == Operation::Download ? QStringLiteral("Hydrate") : QStringLiteral("Dehydrate");
    for (const QString &path : std::as_const(toSend)) {
        QDBusMessage call = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, method);
        call << path;
        m_waiting.insert(path);
        auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(call, CallTimeout), this);
        connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, request, report, path](QDBusPendingCallWatcher *finished) {
            finished->deleteLater();
            m_waiting.remove(path);
            const QDBusPendingReply<> reply = *finished;
            --request->unanswered;
            if (reply.isError()) {
                request->unreported.append({path, reply.error().name(), reply.error().message()});
                // Reported soon, even while other files of the same request
                // are still downloading -- but not one message per file.
                if (!request->timer->isActive()) {
                    request->timer->start();
                }
            }
            if (request->unanswered == 0) {
                report();
                request->timer->deleteLater();
            }
        });
    }
}

} // namespace konedrive
