#pragma once

#include <QDBusConnection>
#include <QDBusPendingCall>
#include <QObject>
#include <QString>
#include <QStringList>
#include <QVariantMap>

#include <functional>

class OrgKonedriveAccounts1Interface;
class QDBusServiceWatcher;

/// konedrived's manager object, /org/konedrive/Accounts
/// (dbus/org.konedrive.Accounts1.xml): the accounts, in the order they were
/// added; the client id every account signs in with; and the helper, which
/// serves every account. Never blocks the GUI thread.
class DaemonController : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool serviceAvailable READ serviceAvailable NOTIFY serviceAvailableChanged)
    /// Every account's object path, in the order they were added. Kept as it
    /// was while the service is away.
    Q_PROPERTY(QStringList accounts READ accounts NOTIFY accountsChanged)
    Q_PROPERTY(QString clientId READ clientId NOTIFY changed)
    /// "connected", "not-installed", "stopped", "failed", "unknown", or empty
    /// before the daemon has said.
    Q_PROPERTY(QString helperState READ helperState NOTIFY changed)
    /// helperState is known and is not "connected".
    Q_PROPERTY(bool helperTrouble READ helperTrouble NOTIFY changed)
    /// What to do about helperState, in one line; empty when there is nothing to add.
    Q_PROPERTY(QString helperInstruction READ helperInstruction NOTIFY changed)
    /// Trouble that belongs to no account (config.toml unreadable, a failed migration).
    Q_PROPERTY(QString lastError READ lastError NOTIFY changed)
    /// Why the last SetClientId asked from this window failed.
    Q_PROPERTY(QString actionError READ actionError NOTIFY actionErrorChanged)
    /// The account a Remove is under way for; empty when none is.
    Q_PROPERTY(QString removing READ removing NOTIFY removeChanged)
    /// The account whose last Remove was refused, and why; empty when none was.
    Q_PROPERTY(QString removeFailedPath READ removeFailedPath NOTIFY removeChanged)
    Q_PROPERTY(QString removeError READ removeError NOTIFY removeChanged)
    /// That refusal was NoHelper: the folder can be forgotten only through the helper.
    Q_PROPERTY(bool removeNeedsHelper READ removeNeedsHelper NOTIFY removeChanged)
    /// That refusal was PendingUploads: changes made here would be lost with the account.
    Q_PROPERTY(bool removeWaitsForUploads READ removeWaitsForUploads NOTIFY removeChanged)

public:
    static const QString ServiceName;
    static const QString ObjectPath;
    static const QString InterfaceName;

    explicit DaemonController(QObject *parent = nullptr);
    explicit DaemonController(const QDBusConnection &bus, QObject *parent = nullptr);

    QDBusConnection bus() const { return m_bus; }
    bool serviceAvailable() const { return m_serviceAvailable; }
    QStringList accounts() const { return m_accounts; }
    QString clientId() const { return m_clientId; }
    QString helperState() const { return m_helperState; }
    bool helperTrouble() const;
    QString helperInstruction() const;
    QString lastError() const { return m_lastError; }
    QString actionError() const { return m_actionError; }
    QString removing() const { return m_removing; }
    QString removeFailedPath() const { return m_removeFailedPath; }
    QString removeError() const { return m_removeError; }
    bool removeNeedsHelper() const { return m_removeNeedsHelper; }
    bool removeWaitsForUploads() const { return m_removeWaitsForUploads; }

    /// Re-reads every Accounts1 property (GetAll).
    Q_INVOKABLE void retry();
    Q_INVOKABLE void setClientId(const QString &id);
    /// SetClientId, then `done`; or `failed` with the daemon's reason. Leaves actionError alone.
    void setClientId(const QString &id, std::function<void()> done, std::function<void(const QString &)> failed);
    /// Add(label): `done` gets the new account's object path, `failed` the daemon's reason.
    void add(const QString &label, std::function<void(const QString &)> done, std::function<void(const QString &)> failed);
    /// Remove(path). Like UnregisterRoot it waits as long as it takes: it
    /// forgets the folder through the helper first. One at a time: asked
    /// while another is under way, it does nothing.
    Q_INVOKABLE void remove(const QString &path);

Q_SIGNALS:
    void serviceAvailableChanged();
    void accountsChanged();
    void changed();
    void actionErrorChanged();
    void removeChanged();

private Q_SLOTS:
    void onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated);

private:
    void fetchAll();
    void applyProperties(const QVariantMap &properties);
    void setServiceAvailable(bool available);
    void setActionError(const QString &message);
    void watch(const QDBusPendingCall &pending, std::function<void()> done, std::function<void(const QString &)> failed);

    QDBusConnection m_bus;
    OrgKonedriveAccounts1Interface *m_iface;
    QDBusServiceWatcher *m_watcher;
    bool m_serviceAvailable = false;
    QStringList m_accounts;
    QString m_clientId;
    QString m_helperState;
    QString m_lastError;
    QString m_actionError;
    QString m_removing;
    QString m_removeFailedPath;
    QString m_removeError;
    bool m_removeNeedsHelper = false;
    bool m_removeWaitsForUploads = false;
};
