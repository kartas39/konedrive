#pragma once

#include <QDBusConnection>
#include <QDBusObjectPath>
#include <QDBusPendingCall>
#include <QObject>
#include <QString>
#include <QStringList>
#include <QVariantMap>

#include <functional>

class OrgKonedriveAccountsInterface;
class QDBusServiceWatcher;

/// konedrived's manager object, /org/konedrive/Accounts
/// (dbus/org.konedrive.Accounts.xml): the accounts, in the order they were
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
    /// The hold settings of the whole app (PauseOnMetered, OnBattery: "sync",
    /// "power-saver" or "pause"): the same for every account.
    Q_PROPERTY(bool pauseOnMetered READ pauseOnMetered NOTIFY changed)
    Q_PROPERTY(QString onBattery READ onBattery NOTIFY changed)
    /// Trouble that belongs to no account (config.toml unreadable, a failed migration).
    Q_PROPERTY(QString lastError READ lastError NOTIFY changed)
    /// Why the last SetClientId, SetPauseOnMetered or SetOnBattery asked from this window failed.
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
    /// This window's build (CMakeLists.txt, docs/releasing.md): X.Y.Z,
    /// X.Y.Z-dev.N or X.Y.Z-dev, and the full hash of its commit or "unknown".
    Q_PROPERTY(QString version READ version CONSTANT)
    Q_PROPERTY(QString commit READ commit CONSTANT)
    /// "Version 0.1.1-dev.57 · commit 5254595": the sidebar's line.
    Q_PROPERTY(QString versionLine READ versionLine CONSTANT)
    /// The running daemon's Version and Commit; empty before it has said, or
    /// from a daemon too old to have them.
    Q_PROPERTY(QString daemonVersion READ daemonVersion NOTIFY changed)
    Q_PROPERTY(QString daemonCommit READ daemonCommit NOTIFY changed)
    /// The daemon is on the bus and runs another build than this window's
    /// (installed, not restarted): what the sidebar says about it, else empty.
    Q_PROPERTY(QString daemonBuildMismatch READ daemonBuildMismatch NOTIFY daemonBuildChanged)

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
    bool pauseOnMetered() const { return m_pauseOnMetered; }
    QString onBattery() const { return m_onBattery; }
    QString lastError() const { return m_lastError; }
    QString actionError() const { return m_actionError; }
    QString removing() const { return m_removing; }
    QString removeFailedPath() const { return m_removeFailedPath; }
    QString removeError() const { return m_removeError; }
    bool removeNeedsHelper() const { return m_removeNeedsHelper; }
    bool removeWaitsForUploads() const { return m_removeWaitsForUploads; }
    QString version() const { return m_version; }
    QString commit() const { return m_commit; }
    QString versionLine() const;
    QString daemonVersion() const { return m_daemonVersion; }
    QString daemonCommit() const { return m_daemonCommit; }
    QString daemonBuildMismatch() const;

    /// A commit as people read it: its first 7 characters.
    static QString shortCommit(const QString &commit) { return commit.left(7); }

    /// Re-reads every Accounts property (GetAll).
    Q_INVOKABLE void retry();
    Q_INVOKABLE void setClientId(const QString &id);
    /// SetPauseOnMetered, SetOnBattery; a refusal lands in actionError.
    Q_INVOKABLE void setPauseOnMetered(bool on);
    Q_INVOKABLE void setOnBattery(const QString &choice);
    /// SetClientId, then `done`; or `failed` with the daemon's reason. Leaves actionError alone.
    void setClientId(const QString &id, std::function<void()> done, std::function<void(const QString &)> failed);
    /// SignIn: `done` gets the sign-in's number and the URL to open, `failed` the
    /// daemon's reason. How the sign-in ends comes out of signInFinished.
    void signIn(std::function<void(uint, const QString &)> done, std::function<void(const QString &)> failed);
    /// CancelSignIn for the sign-in `number`. The daemon never refuses it, and says how the
    /// sign-in ended with signInFinished; `failed` only when the call got no answer.
    void cancelSignIn(uint number, std::function<void(const QString &)> failed);
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
    void daemonBuildChanged();
    /// Accounts.SignInFinished: how the sign-in `number` ended ("signed-in",
    /// "cancelled", "already-added" or "failed"), the daemon's message, and
    /// the account's path: the new account's, the one that has the drive
    /// already, or "/" when there is none to name.
    void signInFinished(uint number, const QString &outcome, const QString &message, const QString &account);

private Q_SLOTS:
    void onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated);
    void onSignInFinished(uint number, const QString &outcome, const QString &message, const QDBusObjectPath &account);

private:
    void fetchAll();
    void applyProperties(const QVariantMap &properties);
    void setServiceAvailable(bool available);
    void setActionError(const QString &message);
    void watch(const QDBusPendingCall &pending, std::function<void()> done, std::function<void(const QString &)> failed);

    QDBusConnection m_bus;
    OrgKonedriveAccountsInterface *m_iface;
    QDBusServiceWatcher *m_watcher;
    bool m_serviceAvailable = false;
    QStringList m_accounts;
    QString m_clientId;
    QString m_helperState;
    bool m_pauseOnMetered = true;
    QString m_onBattery = QStringLiteral("power-saver");
    QString m_lastError;
    QString m_actionError;
    QString m_removing;
    QString m_removeFailedPath;
    QString m_removeError;
    bool m_removeNeedsHelper = false;
    bool m_removeWaitsForUploads = false;
    const QString m_version;
    const QString m_commit;
    QString m_daemonVersion;
    QString m_daemonCommit;
    /// Version and Commit have been read from the running daemon (present or not).
    bool m_daemonBuildKnown = false;
};
