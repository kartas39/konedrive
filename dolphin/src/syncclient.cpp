#include "syncclient.h"

#include <QDBusArgument>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QTimer>

#include <memory>

namespace konedrive
{

const QString SyncClient::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString SyncClient::ObjectPath = QStringLiteral("/org/konedrive/Accounts");
const QString SyncClient::InterfaceName = QStringLiteral("org.konedrive.Files");

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

namespace
{
MenuAnswer::Offer offerFrom(const QString &value)
{
    if (value == QLatin1String("enabled")) {
        return MenuAnswer::Offer::Enabled;
    }
    if (value == QLatin1String("disabled")) {
        return MenuAnswer::Offer::Disabled;
    }
    return MenuAnswer::Offer::Hidden;
}

/// A value of the map as itself: QtDBus leaves a container inside a variant
/// as a QDBusArgument unless it knows the type.
template<typename T>
T valueOf(const QVariant &value)
{
    return value.userType() == qMetaTypeId<QDBusArgument>() ? qdbus_cast<T>(value.value<QDBusArgument>()) : value.value<T>();
}
} // namespace

MenuAnswer menuAnswerFrom(const QVariantMap &answer)
{
    MenuAnswer result;
    result.paths = valueOf<QStringList>(answer.value(QStringLiteral("paths")));
    if (!result.paths.isEmpty()) {
        const QString keep = answer.value(QStringLiteral("always-keep")).toString();
        if (keep == QLatin1String("off")) {
            result.alwaysKeep = MenuAnswer::AlwaysKeep::Off;
        } else if (keep == QLatin1String("on")) {
            result.alwaysKeep = MenuAnswer::AlwaysKeep::On;
        } else if (keep == QLatin1String("on-locked")) {
            result.alwaysKeep = MenuAnswer::AlwaysKeep::OnLocked;
        }
        result.freeUp = offerFrom(answer.value(QStringLiteral("free-up")).toString());
    }
    if (result.freeUp == MenuAnswer::Offer::Disabled) {
        const QString why = answer.value(QStringLiteral("free-up-why")).toString();
        if (why == QLatin1String("pinned-above")) {
            result.freeUpWhy = MenuAnswer::FreeUpWhy::PinnedAbove;
        } else if (why == QLatin1String("no-helper")) {
            result.freeUpWhy = MenuAnswer::FreeUpWhy::NoHelper;
        } else if (why == QLatin1String("not-uploaded")) {
            result.freeUpWhy = MenuAnswer::FreeUpWhy::NotUploaded;
        } else if (why == QLatin1String("unknown")) {
            result.freeUpWhy = MenuAnswer::FreeUpWhy::Unknown;
        }
    }
    result.blockedBy = answer.value(QStringLiteral("blocked-by")).toString();
    result.openOnlinePath = answer.value(QStringLiteral("open-online-path")).toString();
    if (!result.openOnlinePath.isEmpty()) {
        result.openOnline = offerFrom(answer.value(QStringLiteral("open-online")).toString());
    }
    return result;
}

void SyncClient::askMenu(const QStringList &paths, QObject *context, const std::function<void(const std::optional<MenuAnswer> &)> &answered)
{
    QDBusMessage call = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, QStringLiteral("Menu"));
    call << paths;
    // A right click is not a request for the daemon: it is never started for it.
    call.setAutoStartService(false);
    // A call that was never sent is never answered, and no reply timeout runs
    // for it: with no session bus the pending call is empty, and it says it is
    // an error. (So does one the bus has refused already.) Either way the
    // answer is "offers nothing", handed over as any other answer is: later,
    // on the event loop, and only while the context lives.
    if (m_bus.isConnected()) {
        const QDBusPendingCall pending = m_bus.asyncCall(call, MenuAnswerTimeoutMs);
        if (!pending.isError()) {
            // The watcher is the context's: it goes with it, and with it the answer.
            auto *watcher = new QDBusPendingCallWatcher(pending, context);
            connect(watcher, &QDBusPendingCallWatcher::finished, context, [answered](QDBusPendingCallWatcher *finished) {
                finished->deleteLater();
                const QDBusPendingReply<QVariantMap> reply = *finished;
                if (reply.isError()) {
                    answered(std::nullopt);
                } else {
                    answered(menuAnswerFrom(reply.value()));
                }
            });
            return;
        }
    }
    QTimer::singleShot(0, context, [answered]() {
        answered(std::nullopt);
    });
}

void SyncClient::start(Operation operation, const QStringList &paths)
{
    if (paths.isEmpty()) {
        return;
    }

    // WebUrl takes one path; "Open in OneDrive" is never offered for more.
    const QStringList asked = operation == Operation::OpenOnline ? paths.mid(0, 1) : paths;
    QStringList toSend;
    QSet<QString> sending;
    QList<Failure> notSent;
    for (const QString &path : asked) {
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
    case Operation::OpenOnline:
        method = QStringLiteral("WebUrl");
        break;
    }
    QDBusMessage call = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, method);
    if (operation == Operation::OpenOnline) {
        call << toSend.first();
    } else {
        call << toSend;
    }
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
        } else if (operation == Operation::OpenOnline) {
            const QDBusPendingReply<QString> urlReply = *finished;
            Q_EMIT webUrlReady(toSend.first(), urlReply.argumentAt<0>());
        }
        if (request->unanswered == 0) {
            report();
            request->timer->deleteLater();
        }
    });
}

} // namespace konedrive
