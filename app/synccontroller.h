#pragma once

#include "activitymodel.h"
#include "conflictmodel.h"
#include "folderpicker.h"
#include "synctypes.h"
#include "transfermodel.h"
#include "uploadreasons.h"

#include <QDBusConnection>
#include <QDBusPendingCall>
#include <QElapsedTimer>
#include <QObject>
#include <QSet>
#include <QString>
#include <QUrl>
#include <QVariantList>
#include <QVariantMap>

#include <functional>

class OrgKonedriveActivityLogInterface;
class OrgKonedriveConflictsInterface;
class OrgKonedriveFolderInterface;
class OrgKonedriveUploadQueueInterface;
class QDBusServiceWatcher;
class QTimer;

/// Presents one account's folder (at /org/konedrive/Accounts/<id>) to QML: its
/// org.konedrive.Folder, Transfers, UploadQueue, Conflicts, LocalScan and
/// ActivityLog. The helper, which serves every account, is DaemonController's.
/// Never blocks the GUI thread.
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
    /// Changes that need the user before they can go up (see notUploadedSummary).
    Q_PROPERTY(uint blockedCount READ blockedCount NOTIFY syncChanged)
    /// Removals the mass-delete guard holds (HeldCount), waiting for
    /// confirmDeletes() or restoreDeletes().
    Q_PROPERTY(uint heldCount READ heldCount NOTIFY syncChanged)
    /// OneDrive is full (QuotaFull): no content goes up until a Refresh finds space.
    Q_PROPERTY(bool quotaFull READ quotaFull NOTIFY syncChanged)
    /// While full: the changes that wait for space, and the size of their files.
    Q_PROPERTY(uint spaceWaitingCount READ spaceWaitingCount NOTIFY syncChanged)
    Q_PROPERTY(qulonglong spaceWaitingBytes READ spaceWaitingBytes NOTIFY syncChanged)
    /// Files OneDrive refused as too big for the space left.
    Q_PROPERTY(uint tooBigCount READ tooBigCount NOTIFY syncChanged)
    Q_PROPERTY(bool paused READ paused NOTIFY syncChanged)
    /// Unix seconds when the pause ends by itself; 0 while paused until resumed.
    Q_PROPERTY(qlonglong pausedUntil READ pausedUntil NOTIFY syncChanged)
    Q_PROPERTY(QStringList ignorePatterns READ ignorePatterns NOTIFY syncChanged)
    /// Why the account holds back by itself (HeldBack): "metered", "on-battery",
    /// "power-saver", or empty. Never the user's pause, which `paused` shows.
    Q_PROPERTY(QString heldBack READ heldBack NOTIFY syncChanged)
    /// How changes made in OneDrive reach this computer (LiveChanges): "connected" (at once),
    /// "connecting" (the poll runs every minute meanwhile), or "off" (paused, held back, not a
    /// OneDrive folder), which is also what an older daemon without the property reads as.
    Q_PROPERTY(QString liveChanges READ liveChanges NOTIFY syncChanged)
    /// The account's own sync setting (Thumbnails).
    Q_PROPERTY(bool thumbnails READ thumbnails NOTIFY syncChanged)
    /// What a copy of a file changed on both sides is named after: "Report-<machine>.docx".
    Q_PROPERTY(QString machineName READ machineName NOTIFY syncChanged)
    /// Uploads under way (Transfers.Uploads).
    Q_PROPERTY(TransferModel *uploads READ uploads CONSTANT)
    /// The account's transfers and pool (Transfers' DownloadSpeed, UploadSpeed, ActiveDownloads,
    /// ActiveUploads, PoolInUse, PoolSize, PoolCeiling, LargeFiles, LargeStreams,
    /// LargeStreamLimit, RetryAfter): bytes a second, the files moving each way now (each
    /// once), the slots in use (may be above the size), the pool now and its ceiling, the large
    /// files the sync moves, the streams of large sync transfers and their limit
    /// (largeTransfers, largeLimit), and the seconds left of OneDrive's Retry-After (0: none).
    Q_PROPERTY(qulonglong downloadSpeed READ downloadSpeed NOTIFY syncChanged)
    Q_PROPERTY(qulonglong uploadSpeed READ uploadSpeed NOTIFY syncChanged)
    Q_PROPERTY(uint activeDownloads READ activeDownloads NOTIFY syncChanged)
    Q_PROPERTY(uint activeUploads READ activeUploads NOTIFY syncChanged)
    Q_PROPERTY(uint poolInUse READ poolInUse NOTIFY syncChanged)
    Q_PROPERTY(uint poolSize READ poolSize NOTIFY syncChanged)
    Q_PROPERTY(uint poolCeiling READ poolCeiling NOTIFY syncChanged)
    Q_PROPERTY(uint largeFiles READ largeFiles NOTIFY syncChanged)
    Q_PROPERTY(uint largeTransfers READ largeTransfers NOTIFY syncChanged)
    Q_PROPERTY(uint largeLimit READ largeLimit NOTIFY syncChanged)
    Q_PROPERTY(uint retryAfter READ retryAfter NOTIFY syncChanged)
    /// The queue totals, each way (Transfers' DownloadLeftCount, DownloadLeftBytes,
    /// DownloadDoneBytes, DownloadTimeLeft and the same four for uploads): files left to
    /// download and changes left to upload, their bytes, the bytes done in this run, and
    /// the seconds left (0: unknown).
    Q_PROPERTY(uint downloadLeftCount READ downloadLeftCount NOTIFY syncChanged)
    Q_PROPERTY(qulonglong downloadLeftBytes READ downloadLeftBytes NOTIFY syncChanged)
    Q_PROPERTY(qulonglong downloadDoneBytes READ downloadDoneBytes NOTIFY syncChanged)
    Q_PROPERTY(uint downloadTimeLeft READ downloadTimeLeft NOTIFY syncChanged)
    Q_PROPERTY(uint uploadLeftCount READ uploadLeftCount NOTIFY syncChanged)
    Q_PROPERTY(qulonglong uploadLeftBytes READ uploadLeftBytes NOTIFY syncChanged)
    Q_PROPERTY(qulonglong uploadDoneBytes READ uploadDoneBytes NOTIFY syncChanged)
    Q_PROPERTY(uint uploadTimeLeft READ uploadTimeLeft NOTIFY syncChanged)
    /// The Full local scan (LocalScan's properties): "running", "idle", or "none" for a
    /// read-only folder; why it runs; when it started (unix seconds); the directories and
    /// files seen so far; about how many items it will see (the base's count); when the
    /// last one finished (0: not yet) and how long it took, in seconds.
    Q_PROPERTY(QString scanState READ scanState NOTIFY syncChanged)
    Q_PROPERTY(QString scanReason READ scanReason NOTIFY syncChanged)
    Q_PROPERTY(qlonglong scanStarted READ scanStarted NOTIFY syncChanged)
    Q_PROPERTY(qulonglong scanDirectories READ scanDirectories NOTIFY syncChanged)
    Q_PROPERTY(qulonglong scanFiles READ scanFiles NOTIFY syncChanged)
    Q_PROPERTY(qulonglong scanExpected READ scanExpected NOTIFY syncChanged)
    Q_PROPERTY(qlonglong scanFinished READ scanFinished NOTIFY syncChanged)
    Q_PROPERTY(uint scanTook READ scanTook NOTIFY syncChanged)
    /// The last two minutes of each, one sample a second, oldest first: the window keeps
    /// them, the daemon does not.
    Q_PROPERTY(QVariantList downloadSpeedHistory READ downloadSpeedHistory NOTIFY historyChanged)
    Q_PROPERTY(QVariantList uploadSpeedHistory READ uploadSpeedHistory NOTIFY historyChanged)
    Q_PROPERTY(QVariantList activeDownloadsHistory READ activeDownloadsHistory NOTIFY historyChanged)
    Q_PROPERTY(QVariantList activeUploadsHistory READ activeUploadsHistory NOTIFY historyChanged)
    /// What is kept back, one entry per reason: {group, reason, count, bytes, why}
    /// (NotUploadedSummary()), groups in the order one-action, per-file, never, waiting.
    Q_PROPERTY(QVariantList notUploadedSummary READ notUploadedSummary NOTIFY notUploadedChanged)
    /// Whether the summary has been read since the daemon appeared.
    Q_PROPERTY(bool notUploadedKnown READ notUploadedKnown NOTIFY notUploadedChanged)
    /// The size of the blocked changes: the reasons that need the user.
    Q_PROPERTY(qulonglong blockedBytes READ blockedBytes NOTIFY notUploadedChanged)
    /// The files of each reason shown (setNotUploadedFilesShown), at most
    /// perFileCap each: reason → {items: [{path, reason, why}], total}.
    Q_PROPERTY(QVariantMap notUploadedFiles READ notUploadedFiles NOTIFY notUploadedFilesChanged)
    /// How many files of one reason the window lists (issue #20; a guess).
    Q_PROPERTY(int perFileCap READ perFileCap CONSTANT)
    /// The selection (issue #58): true while every folder of OneDrive is synced
    /// (SyncsEverything); else the chosen folders, each {id, path} with the path in
    /// OneDrive, empty for a folder the daemon has not listed yet (SelectedFolders), and
    /// whether the files directly in the root are synced (RootFiles).
    Q_PROPERTY(bool syncsEverything READ syncsEverything NOTIFY syncChanged)
    Q_PROPERTY(QVariantList selectedFolders READ selectedFolders NOTIFY syncChanged)
    Q_PROPERTY(bool rootFiles READ rootFiles NOTIFY syncChanged)
    /// The picker of the chosen folders; open() reads it anew.
    Q_PROPERTY(FolderPicker *picker READ picker CONSTANT)
    /// The folder was bound with "Choose Folders…": the picker is to open once the first
    /// listing has finished (clearChoosePending() when it does).
    Q_PROPERTY(bool choosePending READ choosePending NOTIFY choosePendingChanged)

public:
    static constexpr int PerFileCap = 20;
    static const QString ServiceName;
    static const QString FolderInterface;
    static const QString TransfersInterface;
    static const QString UploadQueueInterface;
    static const QString ConflictsInterface;
    static const QString LocalScanInterface;
    static const QString ActivityLogInterface;
    /// Every interface whose properties the controller shows, each read with GetAll.
    static const QStringList PropertyInterfaces;

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
    bool quotaFull() const { return m_quotaFull; }
    uint spaceWaitingCount() const { return m_spaceWaitingCount; }
    qulonglong spaceWaitingBytes() const { return m_spaceWaitingBytes; }
    uint tooBigCount() const { return m_tooBigCount; }
    bool paused() const { return m_paused; }
    qlonglong pausedUntil() const { return m_pausedUntil; }
    QStringList ignorePatterns() const { return m_ignorePatterns; }
    QString heldBack() const { return m_heldBack; }
    QString liveChanges() const { return m_liveChanges; }
    bool thumbnails() const { return m_thumbnails; }
    QString machineName() const { return m_machineName; }
    TransferModel *uploads() const { return m_uploads; }
    qulonglong downloadSpeed() const { return m_downloadSpeed; }
    qulonglong uploadSpeed() const { return m_uploadSpeed; }
    uint activeDownloads() const { return m_activeDownloads; }
    uint activeUploads() const { return m_activeUploads; }
    uint poolInUse() const { return m_poolInUse; }
    uint poolSize() const { return m_poolSize; }
    uint poolCeiling() const { return m_poolCeiling; }
    uint largeFiles() const { return m_largeFiles; }
    uint largeTransfers() const { return m_largeTransfers; }
    uint largeLimit() const { return m_largeLimit; }
    uint retryAfter() const { return m_retryAfter; }
    uint downloadLeftCount() const { return m_downloadLeftCount; }
    qulonglong downloadLeftBytes() const { return m_downloadLeftBytes; }
    qulonglong downloadDoneBytes() const { return m_downloadDoneBytes; }
    uint downloadTimeLeft() const { return m_downloadTimeLeft; }
    uint uploadLeftCount() const { return m_uploadLeftCount; }
    qulonglong uploadLeftBytes() const { return m_uploadLeftBytes; }
    qulonglong uploadDoneBytes() const { return m_uploadDoneBytes; }
    uint uploadTimeLeft() const { return m_uploadTimeLeft; }
    QString scanState() const { return m_scanState; }
    QString scanReason() const { return m_scanReason; }
    qlonglong scanStarted() const { return m_scanStarted; }
    qulonglong scanDirectories() const { return m_scanDirectories; }
    qulonglong scanFiles() const { return m_scanFiles; }
    qulonglong scanExpected() const { return m_scanExpected; }
    qlonglong scanFinished() const { return m_scanFinished; }
    uint scanTook() const { return m_scanTook; }
    QVariantList downloadSpeedHistory() const { return m_history[0]; }
    QVariantList uploadSpeedHistory() const { return m_history[1]; }
    QVariantList activeDownloadsHistory() const { return m_history[2]; }
    QVariantList activeUploadsHistory() const { return m_history[3]; }
    /// Samples kept in each history: two minutes, one a second.
    static constexpr int HistoryLength = 120;
    QVariantList notUploadedSummary() const { return m_notUploadedSummary; }
    bool notUploadedKnown() const { return m_notUploadedKnown; }
    qulonglong blockedBytes() const { return m_blockedBytes; }
    QVariantMap notUploadedFiles() const { return m_notUploadedFiles; }
    int perFileCap() const { return PerFileCap; }
    bool syncsEverything() const { return m_syncsEverything; }
    QVariantList selectedFolders() const { return m_selectedFolders; }
    bool rootFiles() const { return m_rootFiles; }
    FolderPicker *picker() const { return m_picker; }
    bool choosePending() const { return m_choosePending; }

    /// Folder.Register; a NoHelper refusal is kept as `pendingFolder` for the
    /// window to prompt about — never registered without interception, since
    /// an unhydrated file then reads as zeros for good (no-interception stays
    /// a daemon/CLI-only mode; docs/design/decisions.md).
    Q_INVOKABLE void chooseFolder(const QUrl &folder);
    /// Binds with nothing placed, for the folders to be chosen afterwards: an empty
    /// selection (SetSelection([], false)), then Register. A refused Register takes the
    /// empty selection back (SyncEverything); NoHelper is kept as `pendingFolder`, as in
    /// chooseFolder. Bound, `choosePending` turns on.
    Q_INVOKABLE void chooseFolderAndFolders(const QUrl &folder);
    Q_INVOKABLE void clearChoosePending();
    /// "Try Again" on the NoHelper prompt: retries Register(pendingFolder), the way it
    /// was asked for.
    Q_INVOKABLE void retryRegistration();
    Q_INVOKABLE void cancelPending();
    Q_INVOKABLE void forget();
    Q_INVOKABLE void refresh();
    Q_INVOKABLE void loadSkipped();
    Q_INVOKABLE void openFolder();
    /// ActivityLog.Recent(ActivityModel::Capacity) into `activity`.
    Q_INVOKABLE void loadActivity();
    /// Conflicts.List() into `conflicts`.
    Q_INVOKABLE void loadConflicts();
    /// Conflicts.Dismiss, then List() again.
    Q_INVOKABLE void dismissConflict(const QString &rescuedPath);
    /// FreeUpSpace; the outcome lands in `freeUpResult`.
    Q_INVOKABLE void freeUpSpace();
    Q_INVOKABLE void clearFreeUpResult();
    /// Opens the file manager on the file's folder with the file selected.
    Q_INVOKABLE void showInFolder(const QString &path);
    /// Re-reads every property of the folder's interfaces (GetAll). "Try Again" on the service-down
    /// card calls this alongside AccountController::retry.
    Q_INVOKABLE void retry();

    /// Pause(seconds): 0 pauses until resume().
    Q_INVOKABLE void pause(uint seconds);
    Q_INVOKABLE void resume();
    /// SyncAnyway: the automatic hold is lifted until a source or its setting changes.
    Q_INVOKABLE void syncAnyway();
    /// SetThumbnails; a refusal lands in actionError.
    Q_INVOKABLE void setThumbnails(bool on);
    /// SetIgnorePatterns; a refusal lands in actionError.
    Q_INVOKABLE void setIgnorePatterns(const QStringList &patterns);
    /// The list with `pattern` (trimmed) added, if it is not there yet.
    Q_INVOKABLE void addIgnorePattern(const QString &pattern);
    Q_INVOKABLE void removeIgnorePattern(const QString &pattern);
    /// The mass-delete guard: the held removals go ahead, or are taken back.
    Q_INVOKABLE void confirmDeletes();
    Q_INVOKABLE void restoreDeletes();
    /// NotUploadedSummary() into `notUploadedSummary`, and the files of the
    /// reasons shown into `notUploadedFiles`, quietly; at most once a second
    /// (a call within the second is put off to its end). The pages call it
    /// while they are shown: nothing reloads it on its own.
    Q_INVOKABLE void loadNotUploaded();
    /// The files of `reason` are shown (NotUploadedFiles(reason, perFileCap)
    /// now, and with every loadNotUploaded()), or no longer.
    Q_INVOKABLE void setNotUploadedFilesShown(const QString &reason, bool shown);
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
    void choosePendingChanged();
    void freeUpResultChanged();
    void notUploadedChanged();
    void historyChanged();
    void notUploadedFilesChanged();
    /// One ActivityLog.Added from the daemon, as it happens.
    void activityAdded(qlonglong time, const QString &kind, const QString &path, const QString &detail);

private Q_SLOTS:
    void onPropertiesChanged(const QString &interfaceName, const QVariantMap &changed, const QStringList &invalidated);
    void onActivityAdded(qlonglong time, const QString &kind, const QString &path, const QString &detail);

private:
    void fetchAll();
    /// The properties of `interfaceName` (one of PropertyInterfaces), from GetAll or PropertiesChanged.
    void applyProperties(const QString &interfaceName, const QVariantMap &properties);
    void setServiceAvailable(bool available);
    void setActionError(const QString &message);
    void setPendingFolder(const QString &folder);
    void setChoosePending(bool pending);
    void registerFolder(const QString &path, bool choosing);
    /// `resetError` false (loadSkipped, an incidental background reload) means
    /// this call never clears an actionError already showing (M7): only a
    /// user-started action gets to wipe out the previous one.
    void call(const QDBusPendingCall &pending,
              std::function<void(const QDBusPendingCall &)> onSuccess = {},
              std::function<bool(const QDBusError &)> onError = {},
              bool resetError = true);
    /// A call the user did not ask for (a list refresh): its failure is not shown.
    void quietly(const QDBusPendingCall &pending, std::function<void(const QDBusPendingCall &)> onSuccess);
    void loadNotUploadedFiles(const QString &reason);

    QDBusConnection m_bus;
    QString m_path;
    OrgKonedriveFolderInterface *m_folder;
    OrgKonedriveUploadQueueInterface *m_queue;
    OrgKonedriveConflictsInterface *m_conflictsIface;
    OrgKonedriveActivityLogInterface *m_activityLog;
    /// fetchAll() calls so far: a GetAll answer of an older one is not counted.
    int m_fetches = 0;
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
    bool m_quotaFull = false;
    uint m_spaceWaitingCount = 0;
    qulonglong m_spaceWaitingBytes = 0;
    uint m_tooBigCount = 0;
    bool m_paused = false;
    qlonglong m_pausedUntil = 0;
    QStringList m_ignorePatterns;
    QString m_heldBack;
    QString m_liveChanges = QStringLiteral("off");
    bool m_thumbnails = true;
    QString m_machineName;
    TransferModel *m_uploads;
    /// Adds one sample to each history (every second).
    void sampleHistory();
    qulonglong m_downloadSpeed = 0;
    qulonglong m_uploadSpeed = 0;
    uint m_activeDownloads = 0;
    uint m_activeUploads = 0;
    uint m_poolInUse = 0;
    uint m_poolSize = 0;
    uint m_poolCeiling = 0;
    uint m_largeFiles = 0;
    uint m_largeTransfers = 0;
    uint m_largeLimit = 0;
    uint m_retryAfter = 0;
    uint m_downloadLeftCount = 0;
    qulonglong m_downloadLeftBytes = 0;
    qulonglong m_downloadDoneBytes = 0;
    uint m_downloadTimeLeft = 0;
    uint m_uploadLeftCount = 0;
    qulonglong m_uploadLeftBytes = 0;
    qulonglong m_uploadDoneBytes = 0;
    uint m_uploadTimeLeft = 0;
    QString m_scanState = QStringLiteral("none");
    QString m_scanReason;
    qlonglong m_scanStarted = 0;
    qulonglong m_scanDirectories = 0;
    qulonglong m_scanFiles = 0;
    qulonglong m_scanExpected = 0;
    qlonglong m_scanFinished = 0;
    uint m_scanTook = 0;
    /// Download speed, upload speed, active downloads, active uploads.
    QVariantList m_history[4];
    QTimer *m_sampler;
    QVariantList m_notUploadedSummary;
    bool m_notUploadedKnown = false;
    qulonglong m_blockedBytes = 0;
    QVariantMap m_notUploadedFiles;
    bool m_syncsEverything = true;
    QVariantList m_selectedFolders;
    bool m_rootFiles = true;
    FolderPicker *m_picker;
    bool m_choosePending = false;
    /// The folder waiting in `pendingFolder` was asked for with "Choose Folders…".
    bool m_pendingChooses = false;
    QSet<QString> m_filesShown;
    /// loadNotUploaded() put off to the end of the second since the last one.
    QTimer *m_notUploadedSoon;
    QElapsedTimer m_notUploadedLast;
    /// Recent() calls on their way, and the live events since the first of them.
    int m_activityLoads = 0;
    KonedriveActivityList m_liveDuringLoad;
};
