#pragma once

#include "activitymodel.h"
#include "conflictmodel.h"
#include "outboxmodel.h"
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
class QTimer;

/// Presents one account's org.konedrive.Sync1 (at /org/konedrive/Accounts/<id>)
/// to QML: that account's folder. The helper, which serves every account, is
/// DaemonController's. Never blocks the GUI thread.
class SyncController : public QObject
{
    Q_OBJECT
    Q_PROPERTY(bool serviceAvailable READ serviceAvailable NOTIFY serviceAvailableChanged)
    Q_PROPERTY(QString path READ path CONSTANT)
    Q_PROPERTY(QString rootPath READ rootPath NOTIFY syncChanged)
    Q_PROPERTY(QString rootState READ rootState NOTIFY syncChanged)
    Q_PROPERTY(QString rootSource READ rootSource NOTIFY syncChanged)
    Q_PROPERTY(QString lastError READ lastError NOTIFY syncChanged)
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
    /// How many files and folders carry their own "Always keep on this device" pin.
    Q_PROPERTY(uint pinnedCount READ pinnedCount NOTIFY syncChanged)
    Q_PROPERTY(TransferModel *transfers READ transfers CONSTANT)
    Q_PROPERTY(ActivityModel *activity READ activity CONSTANT)
    Q_PROPERTY(ConflictModel *conflicts READ conflicts CONSTANT)
    /// What the last Free Up Space did, for an inline message; empty when there is nothing to say.
    Q_PROPERTY(QString freeUpResult READ freeUpResult NOTIFY freeUpResultChanged)
    Q_PROPERTY(bool freeingUp READ freeingUp NOTIFY freeUpResultChanged)
    /// Changes waiting to be uploaded (neither blocked nor held), and the size they send.
    Q_PROPERTY(uint pendingCount READ pendingCount NOTIFY syncChanged)
    Q_PROPERTY(qulonglong pendingBytes READ pendingBytes NOTIFY syncChanged)
    /// Changes that need the user before they can go up (see notUploaded).
    Q_PROPERTY(uint blockedCount READ blockedCount NOTIFY syncChanged)
    /// Removals the mass-delete guard holds (HeldCount), waiting for
    /// confirmDeletes() or restoreDeletes().
    Q_PROPERTY(uint heldCount READ heldCount NOTIFY syncChanged)
    Q_PROPERTY(bool paused READ paused NOTIFY syncChanged)
    /// Unix seconds when the pause ends by itself; 0 while paused until resumed.
    Q_PROPERTY(qlonglong pausedUntil READ pausedUntil NOTIFY syncChanged)
    Q_PROPERTY(QStringList ignorePatterns READ ignorePatterns NOTIFY syncChanged)
    /// What a copy of a file changed on both sides is named after: "Report-<machine>.docx".
    Q_PROPERTY(QString machineName READ machineName NOTIFY syncChanged)
    /// Uploads under way (Sync1's Uploads).
    Q_PROPERTY(TransferModel *uploads READ uploads CONSTANT)
    /// The changes waiting to be uploaded (Outbox()).
    Q_PROPERTY(OutboxModel *outbox READ outbox CONSTANT)
    /// What stays on this computer, and why: {path, reason, why} (NotUploaded()).
    Q_PROPERTY(QVariantList notUploaded READ notUploaded NOTIFY notUploadedChanged)

public:
    static const QString ServiceName;
    static const QString InterfaceName;

    explicit SyncController(const QString &path, QObject *parent = nullptr);
    SyncController(const QDBusConnection &bus, const QString &path, QObject *parent = nullptr);

    bool serviceAvailable() const { return m_serviceAvailable; }
    QString path() const { return m_path; }
    QString rootPath() const { return m_rootPath; }
    QString rootState() const { return m_rootState; }
    QString rootSource() const { return m_rootSource; }
    QString lastError() const { return m_lastError; }
    qulonglong itemsListed() const { return m_itemsListed; }
    qulonglong itemsPlaced() const { return m_itemsPlaced; }
    qulonglong skippedCount() const { return m_skippedCount; }
    QVariantList skipped() const { return m_skipped; }
    QString actionError() const { return m_actionError; }
    QString pendingFolder() const { return m_pendingFolder; }
    qlonglong lastChecked() const { return m_lastChecked; }
    qulonglong localBytes() const { return m_localBytes; }
    uint conflictCount() const { return m_conflictCount; }
    uint pinnedCount() const { return m_pinnedCount; }
    TransferModel *transfers() const { return m_transfers; }
    ActivityModel *activity() const { return m_activity; }
    ConflictModel *conflicts() const { return m_conflicts; }
    QString freeUpResult() const { return m_freeUpResult; }
    bool freeingUp() const { return m_freeingUp; }
    uint pendingCount() const { return m_pendingCount; }
    qulonglong pendingBytes() const { return m_pendingBytes; }
    uint blockedCount() const { return m_blockedCount; }
    uint heldCount() const { return m_heldCount; }
    bool paused() const { return m_paused; }
    qlonglong pausedUntil() const { return m_pausedUntil; }
    QStringList ignorePatterns() const { return m_ignorePatterns; }
    QString machineName() const { return m_machineName; }
    TransferModel *uploads() const { return m_uploads; }
    OutboxModel *outbox() const { return m_outbox; }
    QVariantList notUploaded() const { return m_notUploaded; }
    /// Whether the outbox has been read at least once since the daemon appeared.
    bool outboxKnown() const { return m_outboxKnown; }

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
    /// card calls this alongside AccountController::retry.
    Q_INVOKABLE void retry();

    /// Pause(seconds): 0 pauses until resume().
    Q_INVOKABLE void pause(uint seconds);
    Q_INVOKABLE void resume();
    /// SetIgnorePatterns; a refusal lands in actionError.
    Q_INVOKABLE void setIgnorePatterns(const QStringList &patterns);
    /// The list with `pattern` (trimmed) added, if it is not there yet.
    Q_INVOKABLE void addIgnorePattern(const QString &pattern);
    Q_INVOKABLE void removeIgnorePattern(const QString &pattern);
    /// The mass-delete guard: the held removals go ahead, or are taken back.
    Q_INVOKABLE void confirmDeletes();
    Q_INVOKABLE void restoreDeletes();
    /// Outbox(0) into `outbox`, quietly.
    Q_INVOKABLE void loadOutbox();
    /// NotUploaded() into `notUploaded`, quietly.
    Q_INVOKABLE void loadNotUploaded();
    /// Opens the file manager with both files selected: a copy beside its original.
    Q_INVOKABLE void showBoth(const QString &first, const QString &second);


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
    void notUploadedChanged();
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
    QString m_path;
    OrgKonedriveSync1Interface *m_iface;
    QDBusServiceWatcher *m_watcher;
    bool m_serviceAvailable = false;
    QString m_rootPath;
    QString m_rootState = QStringLiteral("none");
    QString m_rootSource;
    QString m_lastError;
    qulonglong m_itemsListed = 0;
    qulonglong m_itemsPlaced = 0;
    qulonglong m_skippedCount = 0;
    QVariantList m_skipped;
    QString m_actionError;
    QString m_pendingFolder;
    qlonglong m_lastChecked = 0;
    qulonglong m_localBytes = 0;
    uint m_conflictCount = 0;
    uint m_pinnedCount = 0;
    TransferModel *m_transfers;
    ActivityModel *m_activity;
    ConflictModel *m_conflicts;
    QString m_freeUpResult;
    bool m_freeingUp = false;
    uint m_pendingCount = 0;
    qulonglong m_pendingBytes = 0;
    uint m_blockedCount = 0;
    uint m_heldCount = 0;
    bool m_paused = false;
    qlonglong m_pausedUntil = 0;
    QStringList m_ignorePatterns;
    QString m_machineName;
    TransferModel *m_uploads;
    OutboxModel *m_outbox;
    bool m_outboxKnown = false;
    QVariantList m_notUploaded;
    /// Reads the outbox again a moment after its counts change.
    QTimer *m_outboxSoon;
    /// RecentActivity() calls on their way, and the live events since the first of them.
    int m_activityLoads = 0;
    KonedriveActivityList m_liveDuringLoad;
};
