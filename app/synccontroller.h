#pragma once

#include "activitymodel.h"
#include "conflictmodel.h"
#include "synctypes.h"
#include "transfermodel.h"

#include <QDBusConnection>
#include <QDBusPendingCall>
#include <QObject>
#include <QString>
#include <QUrl>
#include <QVariantList>
#include <QVariantMap>

#include <functional>

class OrgKonedriveSync1Interface;
class QDBusServiceWatcher;

/// Presents konedrived's org.konedrive.Sync1 to QML. Never blocks the GUI thread.
class SyncController : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool serviceAvailable READ serviceAvailable NOTIFY serviceAvailableChanged)
    Q_PROPERTY(QString rootPath READ rootPath NOTIFY syncChanged)
    Q_PROPERTY(QString rootState READ rootState NOTIFY syncChanged)
    Q_PROPERTY(QString rootSource READ rootSource NOTIFY syncChanged)
    Q_PROPERTY(QString lastError READ lastError NOTIFY syncChanged)
    /// "connected", "not-installed", "stopped", "failed", "unknown", or empty
    /// before the daemon has said (dbus/org.konedrive.Sync1.xml).
    Q_PROPERTY(QString helperState READ helperState NOTIFY syncChanged)
    /// helperState is known and is not "connected".
    Q_PROPERTY(bool helperTrouble READ helperTrouble NOTIFY syncChanged)
    /// What to do about helperState, in one line; empty when there is nothing to add.
    Q_PROPERTY(QString helperInstruction READ helperInstruction NOTIFY syncChanged)
    Q_PROPERTY(qulonglong itemsListed READ itemsListed NOTIFY syncChanged)
    Q_PROPERTY(qulonglong itemsPlaced READ itemsPlaced NOTIFY syncChanged)
    Q_PROPERTY(qulonglong skippedCount READ skippedCount NOTIFY syncChanged)
    Q_PROPERTY(QVariantList skipped READ skipped NOTIFY skippedChanged)
    Q_PROPERTY(QString actionError READ actionError NOTIFY actionErrorChanged)
    Q_PROPERTY(QString pendingFolder READ pendingFolder NOTIFY pendingFolderChanged)
    /// Unix seconds of the last successful check with OneDrive; 0 = never.
    Q_PROPERTY(qlonglong lastChecked READ lastChecked NOTIFY syncChanged)
    /// Bytes the folder's files take on this disk.
    Q_PROPERTY(qulonglong localBytes READ localBytes NOTIFY syncChanged)
    Q_PROPERTY(uint conflictCount READ conflictCount NOTIFY syncChanged)
    Q_PROPERTY(TransferModel *transfers READ transfers CONSTANT)
    Q_PROPERTY(ActivityModel *activity READ activity CONSTANT)
    Q_PROPERTY(ConflictModel *conflicts READ conflicts CONSTANT)
    /// What the last Free Up Space did, for an inline message; empty when there is nothing to say.
    Q_PROPERTY(QString freeUpResult READ freeUpResult NOTIFY freeUpResultChanged)
    Q_PROPERTY(bool freeingUp READ freeingUp NOTIFY freeUpResultChanged)

public:
    static const QString ServiceName;
    static const QString ObjectPath;
    static const QString InterfaceName;

    explicit SyncController(QObject *parent = nullptr);
    explicit SyncController(const QDBusConnection &bus, QObject *parent = nullptr);

    bool serviceAvailable() const { return m_serviceAvailable; }
    QString rootPath() const { return m_rootPath; }
    QString rootState() const { return m_rootState; }
    QString rootSource() const { return m_rootSource; }
    QString lastError() const { return m_lastError; }
    QString helperState() const { return m_helperState; }
    bool helperTrouble() const;
    QString helperInstruction() const;
    qulonglong itemsListed() const { return m_itemsListed; }
    qulonglong itemsPlaced() const { return m_itemsPlaced; }
    qulonglong skippedCount() const { return m_skippedCount; }
    QVariantList skipped() const { return m_skipped; }
    QString actionError() const { return m_actionError; }
    QString pendingFolder() const { return m_pendingFolder; }
    qlonglong lastChecked() const { return m_lastChecked; }
    qulonglong localBytes() const { return m_localBytes; }
    uint conflictCount() const { return m_conflictCount; }
    TransferModel *transfers() const { return m_transfers; }
    ActivityModel *activity() const { return m_activity; }
    ConflictModel *conflicts() const { return m_conflicts; }
    QString freeUpResult() const { return m_freeUpResult; }
    bool freeingUp() const { return m_freeingUp; }

    /// RegisterRoot; a NoHelper refusal is kept as `pendingFolder` for the
    /// window to prompt about — never registered without interception, since
    /// an unhydrated file then reads as zeros for good (no-interception stays
    /// a daemon/CLI-only mode; docs/design/decisions.md).
    Q_INVOKABLE void chooseFolder(const QUrl &folder);
    /// "Try Again" on the NoHelper prompt: retries RegisterRoot(pendingFolder).
    Q_INVOKABLE void retryRegistration();
    Q_INVOKABLE void cancelPending();
    Q_INVOKABLE void forget();
    Q_INVOKABLE void refresh();
    Q_INVOKABLE void loadSkipped();
    Q_INVOKABLE void openFolder();
    /// RecentActivity(ActivityModel::Capacity) into `activity`.
    Q_INVOKABLE void loadActivity();
    /// Conflicts() into `conflicts`.
    Q_INVOKABLE void loadConflicts();
    /// DismissConflict, then Conflicts() again.
    Q_INVOKABLE void dismissConflict(const QString &rescuedPath);
    /// FreeUpSpace; the outcome lands in `freeUpResult`.
    Q_INVOKABLE void freeUpSpace();
    Q_INVOKABLE void clearFreeUpResult();
    /// Opens the file manager on the file's folder with the file selected.
    Q_INVOKABLE void showInFolder(const QString &path);
    /// Re-reads every Sync1 property (GetAll). "Try Again" on the service-down
    /// card calls this alongside AccountController::retry, and "Check Again"
    /// on the helper card calls it alone.
    Q_INVOKABLE void retry();

    /// How long an ordinary call waits for the daemon, in ms (default: D-Bus's
    /// 25 s). FreeUpSpace, which can run for minutes, never gives up.
    void setCallTimeout(int ms);

Q_SIGNALS:
    void serviceAvailableChanged();
    void syncChanged();
    void skippedChanged();
    void actionErrorChanged();
    void pendingFolderChanged();
    void freeUpResultChanged();
    /// One ActivityAdded from the daemon, as it happens.
    void activityAdded(qlonglong time, const QString &kind, const QString &path, const QString &detail);

private Q_SLOTS:
    void onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated);
    void onActivityAdded(qlonglong time, const QString &kind, const QString &path, const QString &detail);

private:
    void fetchAll();
    void applyProperties(const QVariantMap &properties);
    void setServiceAvailable(bool available);
    void setActionError(const QString &message);
    void setPendingFolder(const QString &folder);
    /// `resetError` false (loadSkipped, an incidental background reload) means
    /// this call never clears an actionError already showing (M7): only a
    /// user-started action gets to wipe out the previous one.
    void call(const QDBusPendingCall &pending,
              std::function<void(const QDBusPendingCall &)> onSuccess = {},
              std::function<bool(const QDBusError &)> onError = {},
              bool resetError = true);
    /// A call the user did not ask for (a list refresh): its failure is not shown.
    void quietly(const QDBusPendingCall &pending, std::function<void(const QDBusPendingCall &)> onSuccess);

    QDBusConnection m_bus;
    OrgKonedriveSync1Interface *m_iface;
    QDBusServiceWatcher *m_watcher;
    bool m_serviceAvailable = false;
    QString m_rootPath;
    QString m_rootState = QStringLiteral("none");
    QString m_rootSource;
    QString m_lastError;
    QString m_helperState;
    qulonglong m_itemsListed = 0;
    qulonglong m_itemsPlaced = 0;
    qulonglong m_skippedCount = 0;
    QVariantList m_skipped;
    QString m_actionError;
    QString m_pendingFolder;
    qlonglong m_lastChecked = 0;
    qulonglong m_localBytes = 0;
    uint m_conflictCount = 0;
    TransferModel *m_transfers;
    ActivityModel *m_activity;
    ConflictModel *m_conflicts;
    QString m_freeUpResult;
    bool m_freeingUp = false;
    /// RecentActivity() calls on their way, and the live events since the first of them.
    int m_activityLoads = 0;
    KonedriveActivityList m_liveDuringLoad;
};
