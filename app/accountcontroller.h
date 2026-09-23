#pragma once

#include <QDBusConnection>
#include <QDBusPendingCall>
#include <QObject>
#include <QString>
#include <QStringList>
#include <QVariantMap>

#include <functional>

class OrgKonedriveAccount1Interface;
class QDBusServiceWatcher;

/// Presents konedrived's org.konedrive.Account1 interface to QML. Never blocks the GUI thread.
class AccountController : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool serviceAvailable READ serviceAvailable NOTIFY serviceAvailableChanged)
    Q_PROPERTY(QString state READ state NOTIFY accountChanged)
    Q_PROPERTY(QString lastError READ lastError NOTIFY accountChanged)
    Q_PROPERTY(QString clientId READ clientId NOTIFY accountChanged)
    Q_PROPERTY(QString displayName READ displayName NOTIFY accountChanged)
    Q_PROPERTY(QString email READ email NOTIFY accountChanged)
    Q_PROPERTY(qulonglong quotaUsed READ quotaUsed NOTIFY accountChanged)
    Q_PROPERTY(qulonglong quotaTotal READ quotaTotal NOTIFY accountChanged)
    Q_PROPERTY(QString signInUrl READ signInUrl NOTIFY signInUrlChanged)
    Q_PROPERTY(QString actionError READ actionError NOTIFY actionErrorChanged)

public:
    static const QString ServiceName;
    static const QString ObjectPath;
    static const QString InterfaceName;

    explicit AccountController(QObject *parent = nullptr);
    explicit AccountController(const QDBusConnection &bus, QObject *parent = nullptr);

    bool serviceAvailable() const { return m_serviceAvailable; }
    QString state() const { return m_state; }
    QString lastError() const { return m_lastError; }
    QString clientId() const { return m_clientId; }
    QString displayName() const { return m_displayName; }
    QString email() const { return m_email; }
    qulonglong quotaUsed() const { return m_quotaUsed; }
    qulonglong quotaTotal() const { return m_quotaTotal; }
    QString signInUrl() const { return m_signInUrl; }
    QString actionError() const { return m_actionError; }

    Q_INVOKABLE void retry();
    Q_INVOKABLE void setClientId(const QString &id);
    Q_INVOKABLE void signIn();
    Q_INVOKABLE void cancelSignIn();
    Q_INVOKABLE void signOut();
    Q_INVOKABLE void refreshAccountInfo();
    Q_INVOKABLE void copySignInUrl();

Q_SIGNALS:
    void serviceAvailableChanged();
    void accountChanged();
    void signInUrlChanged();
    void actionErrorChanged();
    /// BeginSignIn returned the authorization URL; the UI opens it in the browser.
    void openUrlRequested(const QString &url);

private Q_SLOTS:
    void onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated);

private:
    void fetchAll();
    void applyProperties(const QVariantMap &properties);
    void setServiceAvailable(bool available);
    void setActionError(const QString &message);
    void call(const QDBusPendingCall &pending, std::function<void(const QDBusPendingCall &)> onSuccess = {});

    QDBusConnection m_bus;
    OrgKonedriveAccount1Interface *m_iface;
    QDBusServiceWatcher *m_watcher;
    bool m_serviceAvailable = false;
    QString m_state = QStringLiteral("signed-out");
    QString m_lastError;
    QString m_clientId;
    QString m_displayName;
    QString m_email;
    qulonglong m_quotaUsed = 0;
    qulonglong m_quotaTotal = 0;
    QString m_signInUrl;
    QString m_actionError;
};
