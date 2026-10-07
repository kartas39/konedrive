#pragma once

#include <QHash>
#include <QObject>
#include <QString>
#include <QStringList>

#include <functional>

class DownloadJob;
class DownloadJobTracker;
class DownloadProgressSettings;
class SyncController;
class TransferModel;
class QTimer;

/// Reports long downloads to Plasma as KJobs, the same way Dolphin's copy
/// progress shows. Watches SyncController's Transfers: a transfer
/// still running 2 s after it first appeared gets its own job (title
/// "Downloading from OneDrive", the file name as the description, bytes done
/// out of total, and speed). A path leaving Transfers does not finish its
/// job at once: it is held for a 1.5 s grace window (the real daemon drops
/// it from Transfers before the ActivityAdded for its failure goes out), so
/// a `failed`/`update-failed` ActivityAdded naming that path inside the
/// window still fails the job with the reason; otherwise it finishes as a
/// plain success once the window ends. An ActivityAdded for a path whose job
/// is still active (not yet gone from Transfers) fails it right away. At
/// most 5 jobs show at once; a 6th and later are summed into one "and N more
/// files" job, which shrinks and finishes as they do (a failure inside that
/// bucket is simply absorbed: no distinct error surfaces for it). The
/// service becoming unavailable (a daemon restart) finishes every visible
/// and overflow job with an error at once, rather than leaving them frozen.
/// Nothing is reported while `settings` says not to (a null `settings` means
/// always on).
///
/// Uploads are reported the same way, by a second controller per account
/// made with Direction::Upload: it watches Uploads, titles its jobs
/// "Uploading to OneDrive", and an `upload-failed` event fails a job.
class DownloadProgressController : public QObject
{
    Q_OBJECT

public:
    enum class Direction {
        Download,
        Upload,
    };

    /// Milliseconds, monotonic.
    using Clock = std::function<qint64()>;
    static constexpr qint64 PromoteAfterMs = 2000;
    static constexpr qint64 RemovalGraceMs = 1500;
    static constexpr int MaxVisibleJobs = 5;

    DownloadProgressController(SyncController *sync,
                                DownloadJobTracker *tracker,
                                DownloadProgressSettings *settings = nullptr,
                                Clock clock = {},
                                QObject *parent = nullptr,
                                Direction direction = Direction::Download);
    ~DownloadProgressController() override;

    /// The timer that promotes and rechecks jobs (tests check that it is armed).
    QTimer *timer() const { return m_timer; }

    /// The account's name for the jobs' titles ("Downloading from OneDrive —
    /// Family"), asked as each job appears; empty (the default) leaves the
    /// title plain — with one account there is nothing to tell apart.
    void setAccountName(std::function<QString()> name);

public Q_SLOTS:
    /// Promotes any transfer that has now run for 2 s. The timer calls this;
    /// tests call it directly with the clock moved forward, instead of
    /// waiting for the timer to really fire.
    void checkNow();

private Q_SLOTS:
    void onTransfersChanged();
    void onActivityAdded(qlonglong time, const QString &kind, const QString &path, const QString &detail);
    void onEnabledChanged();
    void onServiceAvailableChanged();

private:
    struct Entry {
        qint64 firstSeenMs = 0;
        /// Assigned in the order the path was first seen (Transfers' own
        /// row order), so which 5 become visible is deterministic even when
        /// several transfers turn 2 s old in the same tick.
        qint64 seq = 0;
        qulonglong done = 0;
        qulonglong total = 0;
        DownloadJob *job = nullptr; // set once promoted to its own visible job
        bool inOverflow = false;
        /// True once the path left Transfers but is still held for
        /// RemovalGraceMs, waiting to see whether a failure names it.
        bool pendingRemoval = false;
        qint64 removalGraceEndsMs = 0;
    };

    bool enabled() const;
    /// Transfers or Uploads.
    TransferModel *model() const;
    /// A new job, registered, titled for the account.
    DownloadJob *newJob(const QString &name);
    void reconcile();
    void promote(const QString &path, qint64 now);
    void finishPath(const QString &path, bool failed, const QString &reason);
    void promoteFromOverflow();
    void updateOverflowJob();
    void rearm(qint64 now);
    void teardownAll();
    void finishAllWithError(const QString &reason);

    SyncController *m_sync;
    DownloadJobTracker *m_tracker;
    DownloadProgressSettings *m_settings;
    Clock m_clock;
    Direction m_direction;
    std::function<QString()> m_accountName;
    QTimer *m_timer;
    QHash<QString, Entry> m_entries;
    qint64 m_nextSeq = 0;
    QStringList m_visibleOrder;
    QStringList m_overflowOrder;
    DownloadJob *m_overflowJob = nullptr;
};
