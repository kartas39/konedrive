#include "daemoncontroller.h"

#include "accountsinterface.h"

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
const QString DaemonController::InterfaceName = QStringLiteral("org.konedrive.Accounts");

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
    , m_iface(new OrgKonedriveAccountsInterface(ServiceName, ObjectPath, bus, this))
    , m_watcher(new QDBusServiceWatcher(ServiceName, bus, QDBusServiceWatcher::WatchForOwnerChange, this))
    , m_version(QStringLiteral(KONEDRIVE_VERSION))
    , m_commit(QStringLiteral(KONEDRIVE_COMMIT))
{
    connect(this, &DaemonController::serviceAvailableChanged, this, &DaemonController::daemonBuildChanged);
    m_bus.connect(ServiceName,
                  ObjectPath,
                  QStringLiteral("org.freedesktop.DBus.Properties"),
                  QStringLiteral("PropertiesChanged"),
                  this,
                  SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));
    m_bus.connect(ServiceName, ObjectPath, InterfaceName, QStringLiteral("SignInFinished"), this, SLOT(onSignInFinished(uint, QString, QString, QDBusObjectPath)));
    connect(m_watcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        if (newOwner.isEmpty()) {
            m_daemonBuildKnown = false;
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
            m_daemonBuildKnown = false;
            setServiceAvailable(false);
            return;
        }
        const QVariantMap properties = reply.value();
        // A daemon from before Version and Commit has neither: another build.
        m_daemonVersion = properties.value(QStringLiteral("Version")).toString();
        m_daemonCommit = properties.value(QStringLiteral("Commit")).toString();
        m_daemonBuildKnown = true;
        applyProperties(properties);
        Q_EMIT daemonBuildChanged();
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
    text("OnBattery", m_onBattery);
    if (const auto it = p.constFind(QLatin1String("PauseOnMetered")); it != p.constEnd()) {
        m_pauseOnMetered = it->toBool();
    }
    Q_EMIT changed();
    if (const auto it = p.constFind(QLatin1String("List")); it != p.constEnd()) {
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

void DaemonController::setPauseOnMetered(bool on)
{
    setActionError(QString());
    watch(m_iface->SetPauseOnMetered(on), {}, [this](const QString &error) {
        setActionError(error);
    });
}

void DaemonController::setOnBattery(const QString &choice)
{
    setActionError(QString());
    watch(m_iface->SetOnBattery(choice), {}, [this](const QString &error) {
        setActionError(error);
    });
}

void DaemonController::signIn(std::function<void(uint, const QString &)> done, std::function<void(const QString &)> failed)
{
    auto *watcher = new QDBusPendingCallWatcher(m_iface->SignIn(), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [done, failed](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<uint, QString> reply = *w;
        if (reply.isError()) {
            if (failed) {
                failed(reply.error().message());
            }
        } else if (done) {
            done(reply.argumentAt<0>(), reply.argumentAt<1>());
        }
    });
}

void DaemonController::cancelSignIn(uint number, std::function<void(const QString &)> failed)
{
    watch(m_iface->CancelSignIn(number), {}, std::move(failed));
}

void DaemonController::onSignInFinished(uint number, const QString &outcome, const QString &message, const QDBusObjectPath &account)
{
    Q_EMIT signInFinished(number, outcome, message, account.path());
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

QString DaemonController::versionLine() const
{
    return i18nc("@info version, short commit hash", "Version %1 · commit %2", m_version, shortCommit(m_commit));
}

QString DaemonController::daemonBuildMismatch() const
{
    if (!m_serviceAvailable || !m_daemonBuildKnown || (m_daemonVersion == m_version && m_daemonCommit == m_commit)) {
        return QString();
    }
    if (m_daemonVersion.isEmpty()) {
        return i18nc("@info", "Service: an older version — restart it to use this version");
    }
    return i18nc("@info the running service's version, short commit hash",
                 "Service: %1 · commit %2 — restart it to use this version",
                 m_daemonVersion,
                 shortCommit(m_daemonCommit));
}
