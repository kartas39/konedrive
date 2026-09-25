#include "daemoncontroller.h"

#include "accounts1interface.h"

#include <QDBusArgument>
#include <QDBusError>
#include <QDBusMessage>
#include <QDBusObjectPath>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>

#include <KLocalizedString>

#include <limits>

const QString DaemonController::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString DaemonController::ObjectPath = QStringLiteral("/org/konedrive/Accounts");
const QString DaemonController::InterfaceName = QStringLiteral("org.konedrive.Accounts1");

namespace
{
/// An `ao` inside a{sv} arrives as a QDBusArgument.
QStringList objectPaths(const QVariant &value)
{
    const QList<QDBusObjectPath> paths =
        value.canConvert<QDBusArgument>() ? qdbus_cast<QList<QDBusObjectPath>>(value.value<QDBusArgument>()) : value.value<QList<QDBusObjectPath>>();
    QStringList result;
    result.reserve(paths.size());
    for (const QDBusObjectPath &path : paths) {
        result << path.path();
    }
    return result;
}
}

DaemonController::DaemonController(QObject *parent)
    : DaemonController(QDBusConnection::sessionBus(), parent)
{
}

DaemonController::DaemonController(const QDBusConnection &bus, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_iface(new OrgKonedriveAccounts1Interface(ServiceName, ObjectPath, bus, this))
    , m_watcher(new QDBusServiceWatcher(ServiceName, bus, QDBusServiceWatcher::WatchForOwnerChange, this))
{
    m_bus.connect(ServiceName,
                  ObjectPath,
                  QStringLiteral("org.freedesktop.DBus.Properties"),
                  QStringLiteral("PropertiesChanged"),
                  this,
                  SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));
    connect(m_watcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        if (newOwner.isEmpty()) {
            setServiceAvailable(false);
        } else {
            fetchAll();
        }
    });
    fetchAll();
}

void DaemonController::retry()
{
    fetchAll();
}

void DaemonController::fetchAll()
{
    auto message = QDBusMessage::createMethodCall(ServiceName, ObjectPath, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("GetAll"));
    message << InterfaceName;
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<QVariantMap> reply = *w;
        if (reply.isError()) {
            setServiceAvailable(false);
            return;
        }
        applyProperties(reply.value());
        setServiceAvailable(true);
    });
}

void DaemonController::onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated)
{
    if (interfaceName != InterfaceName) {
        return;
    }
    applyProperties(changed);
    if (!invalidated.isEmpty()) {
        fetchAll();
    }
}

void DaemonController::applyProperties(const QVariantMap &p)
{
    const auto text = [&p](const char *key, QString &field) {
        if (const auto it = p.constFind(QLatin1String(key)); it != p.constEnd()) {
            field = it->toString();
        }
    };
    text("ClientId", m_clientId);
    text("HelperState", m_helperState);
    text("LastError", m_lastError);
    Q_EMIT changed();
    if (const auto it = p.constFind(QLatin1String("Accounts")); it != p.constEnd()) {
        const QStringList accounts = objectPaths(*it);
        if (accounts != m_accounts) {
            m_accounts = accounts;
            Q_EMIT accountsChanged();
        }
    }
}

void DaemonController::setServiceAvailable(bool available)
{
    if (m_serviceAvailable == available) {
        return;
    }
    m_serviceAvailable = available;
    Q_EMIT serviceAvailableChanged();
}

void DaemonController::setActionError(const QString &message)
{
    if (m_actionError == message) {
        return;
    }
    m_actionError = message;
    Q_EMIT actionErrorChanged();
}

void DaemonController::watch(const QDBusPendingCall &pending, std::function<void()> done, std::function<void(const QString &)> failed)
{
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [done, failed](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (w->isError()) {
            if (failed) {
                failed(w->error().message());
            }
        } else if (done) {
            done();
        }
    });
}

void DaemonController::setClientId(const QString &id)
{
    setActionError(QString());
    setClientId(id, {}, [this](const QString &error) {
        setActionError(error);
    });
}

void DaemonController::setClientId(const QString &id, std::function<void()> done, std::function<void(const QString &)> failed)
{
    watch(m_iface->SetClientId(id.trimmed()), std::move(done), std::move(failed));
}

void DaemonController::add(const QString &label, std::function<void(const QString &)> done, std::function<void(const QString &)> failed)
{
    auto *watcher = new QDBusPendingCallWatcher(m_iface->Add(label), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [done, failed](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<QDBusObjectPath> reply = *w;
        if (reply.isError()) {
            if (failed) {
                failed(reply.error().message());
            }
        } else if (done) {
            done(reply.value().path());
        }
    });
}

void DaemonController::remove(const QString &path)
{
    if (!m_removing.isEmpty()) {
        return;
    }
    m_removing = path;
    m_removeFailedPath.clear();
    m_removeError.clear();
    m_removeNeedsHelper = false;
    m_removeWaitsForUploads = false;
    Q_EMIT removeChanged();

    auto message = QDBusMessage::createMethodCall(ServiceName, ObjectPath, InterfaceName, QStringLiteral("Remove"));
    message << QVariant::fromValue(QDBusObjectPath(path));
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message, std::numeric_limits<int>::max()), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, path](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        m_removing.clear();
        if (w->isError()) {
            m_removeFailedPath = path;
            m_removeError = w->error().message();
            m_removeNeedsHelper = w->error().name() == QLatin1String("org.konedrive.Error.NoHelper");
            m_removeWaitsForUploads = w->error().name() == QLatin1String("org.konedrive.Error.PendingUploads");
        }
        Q_EMIT removeChanged();
    });
}

bool DaemonController::helperTrouble() const
{
    return !m_helperState.isEmpty() && m_helperState != QLatin1String("connected");
}

QString DaemonController::helperInstruction() const
{
    if (m_helperState == QLatin1String("not-installed")) {
        return i18n("Install the helper: sudo scripts/install-helper.sh (see README)");
    }
    if (m_helperState == QLatin1String("stopped")) {
        return i18n("sudo systemctl start konedrive-helper");
    }
    if (m_helperState == QLatin1String("failed")) {
        return i18n("systemctl status konedrive-helper shows why");
    }
    if (m_helperState == QLatin1String("unknown")) {
        return i18n("The daemon cannot reach the helper.");
    }
    return QString();
}
