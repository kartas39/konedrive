#include "accountcontroller.h"

#include "account1interface.h"

#include <QClipboard>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QGuiApplication>

const QString AccountController::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString AccountController::ObjectPath = QStringLiteral("/org/konedrive/Daemon");
const QString AccountController::InterfaceName = QStringLiteral("org.konedrive.Account1");

AccountController::AccountController(QObject *parent)
    : AccountController(QDBusConnection::sessionBus(), parent)
{
}

AccountController::AccountController(const QDBusConnection &bus, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_iface(new OrgKonedriveAccount1Interface(ServiceName, ObjectPath, bus, this))
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

void AccountController::retry()
{
    fetchAll();
}

void AccountController::fetchAll()
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

void AccountController::onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated)
{
    if (interfaceName != InterfaceName) {
        return;
    }
    applyProperties(changed);
    if (!invalidated.isEmpty()) {
        fetchAll();
    }
}

void AccountController::applyProperties(const QVariantMap &properties)
{
    const QString previousState = m_state;
    const auto take = [&properties](const QString &key, QString &field) {
        const auto it = properties.constFind(key);
        if (it != properties.constEnd()) {
            field = it->toString();
        }
    };
    take(QStringLiteral("State"), m_state);
    take(QStringLiteral("LastError"), m_lastError);
    take(QStringLiteral("ClientId"), m_clientId);
    take(QStringLiteral("DisplayName"), m_displayName);
    take(QStringLiteral("Email"), m_email);
    if (const auto it = properties.constFind(QStringLiteral("QuotaUsed")); it != properties.constEnd()) {
        m_quotaUsed = it->toULongLong();
    }
    if (const auto it = properties.constFind(QStringLiteral("QuotaTotal")); it != properties.constEnd()) {
        m_quotaTotal = it->toULongLong();
    }
    if (previousState == QLatin1String("signing-in") && m_state != QLatin1String("signing-in") && !m_signInUrl.isEmpty()) {
        m_signInUrl.clear();
        Q_EMIT signInUrlChanged();
    }
    Q_EMIT accountChanged();
}

void AccountController::setServiceAvailable(bool available)
{
    if (m_serviceAvailable == available) {
        return;
    }
    m_serviceAvailable = available;
    Q_EMIT serviceAvailableChanged();
}

void AccountController::setActionError(const QString &message)
{
    if (m_actionError == message) {
        return;
    }
    m_actionError = message;
    Q_EMIT actionErrorChanged();
}

void AccountController::call(const QDBusPendingCall &pending, std::function<void(const QDBusPendingCall &)> onSuccess)
{
    setActionError(QString());
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, onSuccess](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (w->isError()) {
            setActionError(w->error().message());
            return;
        }
        if (onSuccess) {
            onSuccess(*w);
        }
    });
}

void AccountController::setClientId(const QString &id)
{
    call(m_iface->SetClientId(id.trimmed()));
}

void AccountController::signIn()
{
    call(m_iface->BeginSignIn(), [this](const QDBusPendingCall &pending) {
        const QDBusPendingReply<QString> reply = pending;
        m_signInUrl = reply.value();
        Q_EMIT signInUrlChanged();
        Q_EMIT openUrlRequested(m_signInUrl);
    });
}

void AccountController::cancelSignIn()
{
    call(m_iface->CancelSignIn());
}

void AccountController::signOut()
{
    Q_EMIT signOutRequested();
    call(m_iface->SignOut());
}

void AccountController::refreshAccountInfo()
{
    call(m_iface->RefreshAccountInfo());
}

void AccountController::copySignInUrl()
{
    if (auto *clipboard = QGuiApplication::clipboard(); clipboard && !m_signInUrl.isEmpty()) {
        clipboard->setText(m_signInUrl);
    }
}
