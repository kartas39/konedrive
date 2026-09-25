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

/// Presents one account's org.konedrive.Account1 (at /org/konedrive/Accounts/<id>)
/// to QML. Never blocks the GUI thread.
class AccountController : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool serviceAvailable READ serviceAvailable NOTIFY serviceAvailableChanged)
    Q_PROPERTY(QString path READ path CONSTANT)
    /// The last element of the path: 12 lowercase hex characters.
    Q_PROPERTY(QString id READ id CONSTANT)
    /// What people see and type; the id until the daemon has said.
    Q_PROPERTY(QString label READ label NOTIFY accountChanged)
    /// The mode the account runs in (Account1.Mode): "read-only", or "read-write" while
    /// changes made on this computer are uploaded.
    Q_PROPERTY(QString mode READ mode NOTIFY accountChanged)
    /// The mode a switch under way goes to, empty when none is: from setMode() until the
    /// daemon has answered and its properties have been read again, and for "read-write"
    /// on while its sign-in waits in the browser (modeSignInPending).
    Q_PROPERTY(QString switchingTo READ switchingTo NOTIFY modeSwitchChanged)
    /// A switch to read-write waits for its sign-in, whose link is signInUrl: until Mode
    /// turns "read-write", LastError says why not, the account leaves "signed-in", or
    /// cancelModeSwitch(). Account1 says nothing while it waits (limitations log F64).
    Q_PROPERTY(bool modeSignInPending READ modeSignInPending NOTIFY modeSwitchChanged)
    Q_PROPERTY(QString state READ state NOTIFY accountChanged)
    Q_PROPERTY(QString lastError READ lastError NOTIFY accountChanged)
    Q_PROPERTY(QString displayName READ displayName NOTIFY accountChanged)
    Q_PROPERTY(QString email READ email NOTIFY accountChanged)
    Q_PROPERTY(qulonglong quotaUsed READ quotaUsed NOTIFY accountChanged)
    Q_PROPERTY(qulonglong quotaTotal READ quotaTotal NOTIFY accountChanged)
    Q_PROPERTY(QString signInUrl READ signInUrl NOTIFY signInUrlChanged)
    Q_PROPERTY(QString actionError READ actionError NOTIFY actionErrorChanged)

public:
    static const QString ServiceName;
    static const QString InterfaceName;

    explicit AccountController(const QString &path, QObject *parent = nullptr);
    AccountController(const QDBusConnection &bus, const QString &path, QObject *parent = nullptr);

    bool serviceAvailable() const { return m_serviceAvailable; }
    QString path() const { return m_path; }
    QString id() const { return m_id; }
    QString label() const { return m_label.isEmpty() ? m_id : m_label; }
    QString mode() const { return m_mode; }
    QString switchingTo() const { return m_switchingTo; }
    bool modeSignInPending() const { return m_switchingTo == QLatin1String("read-write") && !m_signInUrl.isEmpty(); }
    QString state() const { return m_state; }
    QString lastError() const { return m_lastError; }
    QString displayName() const { return m_displayName; }
    QString email() const { return m_email; }
    qulonglong quotaUsed() const { return m_quotaUsed; }
    qulonglong quotaTotal() const { return m_quotaTotal; }
    QString signInUrl() const { return m_signInUrl; }
    QString actionError() const { return m_actionError; }

    Q_INVOKABLE void retry();
    /// SetLabel; the daemon checks it (Accounts1.Add's rules).
    Q_INVOKABLE void setLabel(const QString &label);
    Q_INVOKABLE void signIn();
    Q_INVOKABLE void cancelSignIn();
    Q_INVOKABLE void signOut();
    Q_INVOKABLE void refreshAccountInfo();
    Q_INVOKABLE void copySignInUrl();
    /// SetMode(mode, force). To read-write, the sign-in URL it answers is opened like
    /// signIn()'s, and the switch waits for it (modeSignInPending). To read-only, a refusal
    /// for changes still waiting to upload is pendingUploadsRefused(), not an actionError.
    Q_INVOKABLE void setMode(const QString &mode, bool force = false);
    /// Gives up a switch to read-write waiting for its sign-in (CancelSignIn).
    Q_INVOKABLE void cancelModeSwitch();

    /// What the window says when SetMode(`mode`) was refused `errorName`, the daemon's
    /// `message` kept only where no plain words fit.
    static QString modeRefusalText(const QString &mode, const QString &errorName, const QString &message);

Q_SIGNALS:
    void serviceAvailableChanged();
    void accountChanged();
    void signInUrlChanged();
    void actionErrorChanged();
    void modeSwitchChanged();
    /// SetMode("read-only") was refused because changes made here still wait to be
    /// uploaded: the window asks whether to drop them, then calls setMode("read-only", true).
    void pendingUploadsRefused();
    /// BeginSignIn returned the authorization URL; the UI opens it in the browser.
    void openUrlRequested(const QString &url);
    /// The user asked to sign out from this window (the Notifier stays quiet about it).
    void signOutRequested();

private Q_SLOTS:
    void onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated);

private:
    /// GetAll; `then` runs once its answer is applied.
    void fetchAll(std::function<void()> then = {});
    void applyProperties(const QVariantMap &properties);
    void setServiceAvailable(bool available);
    void endModeSwitch();
    void setActionError(const QString &message);
    void call(const QDBusPendingCall &pending, std::function<void(const QDBusPendingCall &)> onSuccess = {});

    QDBusConnection m_bus;
    QString m_path;
    QString m_id;
    OrgKonedriveAccount1Interface *m_iface;
    QDBusServiceWatcher *m_watcher;
    bool m_serviceAvailable = false;
    QString m_label;
    QString m_mode = QStringLiteral("read-only");
    QString m_state = QStringLiteral("signed-out");
    QString m_lastError;
    QString m_displayName;
    QString m_email;
    qulonglong m_quotaUsed = 0;
    qulonglong m_quotaTotal = 0;
    QString m_signInUrl;
    QString m_actionError;
    QString m_switchingTo;
    /// Counts setMode() and cancelModeSwitch() calls: an answer to an older one is ignored.
    quint64 m_modeCall = 0;
};
