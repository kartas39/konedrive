#include "accountcontroller.h"

#include "account1interface.h"

#include <KLocalizedString>

#include <QClipboard>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QGuiApplication>

const QString AccountController::ServiceName = QStringLiteral("org.konedrive.Daemon");
const QString AccountController::InterfaceName = QStringLiteral("org.konedrive.Account1");

namespace
{
const QLatin1String ReadWrite("read-write");
const QLatin1String SignedIn("signed-in");
}

AccountController::AccountController(const QString &path, QObject *parent)
    : AccountController(QDBusConnection::sessionBus(), path, parent)
{
}

AccountController::AccountController(const QDBusConnection &bus, const QString &path, QObject *parent)
    : QObject(parent)
    , m_bus(bus)
    , m_path(path)
    , m_id(path.section(QLatin1Char('/'), -1))
    , m_iface(new OrgKonedriveAccount1Interface(ServiceName, path, bus, this))
    , m_watcher(new QDBusServiceWatcher(ServiceName, bus, QDBusServiceWatcher::WatchForOwnerChange, this))
{
    m_bus.connect(ServiceName,
                  m_path,
                  QStringLiteral("org.freedesktop.DBus.Properties"),
                  QStringLiteral("PropertiesChanged"),
                  this,
                  SLOT(onPropertiesChanged(QString, QVariantMap, QStringList)));
    connect(m_watcher, &QDBusServiceWatcher::serviceOwnerChanged, this, [this](const QString &, const QString &, const QString &newOwner) {
        if (newOwner.isEmpty()) {
            setServiceAvailable(false);
            // The daemon that comes back knows nothing of it.
            ++m_modeCall;
            endModeSwitch();
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

void AccountController::fetchAll(std::function<void()> then)
{
    auto message = QDBusMessage::createMethodCall(ServiceName, m_path, QStringLiteral("org.freedesktop.DBus.Properties"), QStringLiteral("GetAll"));
    message << InterfaceName;
    auto *watcher = new QDBusPendingCallWatcher(m_bus.asyncCall(message), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, then](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        const QDBusPendingReply<QVariantMap> reply = *w;
        if (reply.isError()) {
            setServiceAvailable(false);
            return;
        }
        applyProperties(reply.value());
        setServiceAvailable(true);
        if (then) {
            then();
        }
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
    take(QStringLiteral("Label"), m_label);
    take(QStringLiteral("Mode"), m_mode);
    take(QStringLiteral("State"), m_state);
    take(QStringLiteral("LastError"), m_lastError);
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
    // A switch to read-write ends as the command line's does (F64): granted, or LastError
    // saying why not (SetMode cleared it before it answered), or no longer signed in.
    if (modeSignInPending()) {
        const bool refused = properties.contains(QStringLiteral("LastError")) && !m_lastError.isEmpty();
        if (m_mode == ReadWrite || refused || m_state != SignedIn) {
            ++m_modeCall;
            endModeSwitch();
        }
    }
    Q_EMIT accountChanged();
}

void AccountController::endModeSwitch()
{
    if (m_switchingTo.isEmpty()) {
        return;
    }
    const bool hadUrl = modeSignInPending();
    m_switchingTo.clear();
    if (hadUrl) {
        m_signInUrl.clear();
        Q_EMIT signInUrlChanged();
    }
    Q_EMIT modeSwitchChanged();
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

void AccountController::setLabel(const QString &label)
{
    call(m_iface->SetLabel(label.trimmed()));
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

void AccountController::setMode(const QString &mode, bool force)
{
    const quint64 call = ++m_modeCall;
    endModeSwitch();
    m_switchingTo = mode;
    Q_EMIT modeSwitchChanged();
    setActionError(QString());
    auto *watcher = new QDBusPendingCallWatcher(m_iface->SetMode(mode, force), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, call, mode](QDBusPendingCallWatcher *w) {
        w->deleteLater();
        if (call != m_modeCall) {
            return; // given up, or another switch asked for since
        }
        const QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            endModeSwitch();
            if (reply.error().name() == QLatin1String("org.konedrive.Error.PendingUploads")) {
                Q_EMIT pendingUploadsRefused();
            } else {
                setActionError(modeRefusalText(mode, reply.error().name(), reply.error().message()));
            }
            return;
        }
        const QString url = reply.value();
        if (!url.isEmpty()) {
            m_signInUrl = url;
            Q_EMIT signInUrlChanged();
            Q_EMIT modeSwitchChanged();
            Q_EMIT openUrlRequested(url);
        }
        // What SetMode left (LastError cleared; a switch to read-only made) is read again
        // now, rather than waiting for its PropertiesChanged, which may come later.
        fetchAll([this, call] {
            if (call == m_modeCall && !modeSignInPending()) {
                endModeSwitch();
            }
        });
    });
}

void AccountController::cancelModeSwitch()
{
    if (!modeSignInPending()) {
        return;
    }
    ++m_modeCall;
    endModeSwitch();
    call(m_iface->CancelSignIn());
}

QString AccountController::modeRefusalText(const QString &mode, const QString &errorName, const QString &message)
{
    const QLatin1String prefix("org.konedrive.Error.");
    const QString name = errorName.startsWith(prefix) ? errorName.mid(prefix.size()) : QString();
    if (name == QLatin1String("WritesNotAllowed")) {
        // The development gate (F60): nothing the user did, or can undo.
        return i18n("Uploading is not available for this account in this version. While uploading is being developed, only test accounts can upload. Nothing was changed.");
    }
    if (name == QLatin1String("NotSignedIn")) {
        return i18n("This account is not signed in. Sign in first, then turn on uploading.");
    }
    if (name == QLatin1String("ModeNotGranted")) {
        return i18n("This account's sign-in does not allow KOneDrive to change your files. Sign in again: turn on uploading once more, and allow it when Microsoft asks.");
    }
    const QString detail = message.isEmpty() ? errorName : message;
    if (mode == ReadWrite) {
        return i18n("Uploading was not turned on: %1", detail);
    }
    return i18n("Uploading was not turned off: %1", detail);
}
