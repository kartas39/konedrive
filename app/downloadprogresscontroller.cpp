#include "downloadprogresscontroller.h"

#include "downloadjob.h"
#include "downloadjobtracker.h"
#include "downloadprogresssettings.h"
#include "synccontroller.h"
#include "transfermodel.h"

#include <KJob>
#include <KLocalizedString>

#include <QAbstractItemModel>
#include <QElapsedTimer>
#include <QFileInfo>
#include <QSet>
#include <QTimer>

#include <algorithm>
#include <limits>
#include <memory>

DownloadProgressController::DownloadProgressController(SyncController *sync,
                                                         DownloadJobTracker *tracker,
                                                         DownloadProgressSettings *settings,
                                                         Clock clock,
                                                         QObject *parent)
    : QObject(parent)
    , m_sync(sync)
    , m_tracker(tracker)
    , m_settings(settings)
    , m_clock(std::move(clock))
    , m_timer(new QTimer(this))
{
    if (!m_clock) {
        auto elapsed = std::make_shared<QElapsedTimer>();
        elapsed->start();
        m_clock = [elapsed] {
            return elapsed->elapsed();
        };
    }
    m_timer->setSingleShot(true);
    connect(m_timer, &QTimer::timeout, this, &DownloadProgressController::checkNow);

    auto *model = m_sync->transfers();
    connect(model, &QAbstractItemModel::rowsInserted, this, &DownloadProgressController::onTransfersChanged);
    connect(model, &QAbstractItemModel::rowsRemoved, this, &DownloadProgressController::onTransfersChanged);
    connect(model, &QAbstractItemModel::dataChanged, this, &DownloadProgressController::onTransfersChanged);
    connect(m_sync, &SyncController::activityAdded, this, &DownloadProgressController::onActivityAdded);
    connect(m_sync, &SyncController::serviceAvailableChanged, this, &DownloadProgressController::onServiceAvailableChanged);
    if (m_settings) {
        connect(m_settings, &DownloadProgressSettings::enabledChanged, this, &DownloadProgressController::onEnabledChanged);
    }

    reconcile();
}

DownloadProgressController::~DownloadProgressController()
{
    teardownAll();
}

bool DownloadProgressController::enabled() const
{
    return !m_settings || m_settings->enabled();
}

void DownloadProgressController::setAccountName(std::function<QString()> name)
{
    m_accountName = std::move(name);
}

DownloadJob *DownloadProgressController::newJob(const QString &name)
{
    auto *job = new DownloadJob(this);
    job->setObjectName(name);
    if (const QString account = m_accountName ? m_accountName() : QString(); !account.isEmpty()) {
        job->setTitle(i18nc("@title job, %1 is the account's name", "Downloading from OneDrive — %1", account));
    }
    // Registered before the description/progress go out: KUiServerV2JobTracker
    // connects to KJob::description only at the end of registerJob(), so a
    // description emitted earlier is dropped (I1).
    m_tracker->registerJob(job);
    return job;
}

void DownloadProgressController::onTransfersChanged()
{
    reconcile();
}

void DownloadProgressController::onEnabledChanged()
{
    if (enabled()) {
        reconcile();
    } else {
        teardownAll();
    }
}

void DownloadProgressController::onServiceAvailableChanged()
{
    if (!m_sync->serviceAvailable()) {
        // A daemon restart: every visible and overflow job (including ones
        // held in the removal grace window) ends now, as a failure, rather
        // than being left frozen and later "finished" as a success.
        finishAllWithError(i18n("the KOneDrive service stopped"));
    }
}

void DownloadProgressController::onActivityAdded(qlonglong, const QString &kind, const QString &path, const QString &detail)
{
    if (!enabled()) {
        return;
    }
    if (kind != QLatin1String("failed") && kind != QLatin1String("update-failed")) {
        return;
    }
    // Whichever of the two arrives first wins: an ActivityAdded that names a
    // tracked path finishes it with an error right away; a Transfers update
    // that removes an untouched path finishes it as a plain success. A
    // failure that arrives after its path is already gone is a no-op.
    finishPath(path, true, detail);
}

void DownloadProgressController::reconcile()
{
    if (!enabled()) {
        return;
    }
    const qint64 now = m_clock();
    auto *model = m_sync->transfers();

    // Model row order (Transfers' own order), so a path's `seq` reflects
    // when it was first seen relative to the others: deterministic, unlike
    // iterating a QHash.
    QSet<QString> present;
    for (int row = 0; row < model->rowCount(); ++row) {
        const QModelIndex idx = model->index(row, 0);
        const QString path = idx.data(TransferModel::PathRole).toString();
        const qulonglong done = idx.data(TransferModel::DoneRole).toULongLong();
        const qulonglong total = idx.data(TransferModel::TotalRole).toULongLong();
        present.insert(path);

        auto eit = m_entries.find(path);
        if (eit == m_entries.end()) {
            Entry entry;
            entry.firstSeenMs = now;
            entry.seq = m_nextSeq++;
            entry.done = done;
            entry.total = total;
            m_entries.insert(path, entry);
            continue;
        }
        if (eit->pendingRemoval) {
            // Reappeared inside its grace window: back to normal tracking.
            eit->pendingRemoval = false;
            eit->removalGraceEndsMs = 0;
        }
        eit->done = done;
        eit->total = total;
        if (eit->job) {
            eit->job->updateProgress(done, total, now);
        }
    }

    QStringList gone;
    for (auto it = m_entries.cbegin(); it != m_entries.cend(); ++it) {
        if (!present.contains(it.key()) && !it->pendingRemoval) {
            gone << it.key();
        }
    }
    for (const QString &path : std::as_const(gone)) {
        auto it = m_entries.find(path);
        if (it == m_entries.end()) {
            continue;
        }
        if (it->job || it->inOverflow) {
            // Shown already: hold it, since the real daemon drops a failed
            // transfer from Transfers before ActivityAdded(failed) for it
            // goes out. finishPath (via checkNow or a matching activity)
            // decides success or failure once the window is settled.
            it->pendingRemoval = true;
            it->removalGraceEndsMs = now + RemovalGraceMs;
        } else {
            // Never shown (younger than 2 s): nothing to hold.
            finishPath(path, false, QString());
        }
    }

    if (!m_overflowOrder.isEmpty()) {
        updateOverflowJob();
    }
    rearm(now);
}

void DownloadProgressController::checkNow()
{
    if (!enabled()) {
        m_timer->stop();
        return;
    }
    const qint64 now = m_clock();
    QStringList due;
    for (auto it = m_entries.cbegin(); it != m_entries.cend(); ++it) {
        if (!it->pendingRemoval && !it->job && !it->inOverflow && now - it->firstSeenMs >= PromoteAfterMs) {
            due << it.key();
        }
    }
    // Earliest-seen transfers claim the visible slots first.
    std::sort(due.begin(), due.end(), [this](const QString &a, const QString &b) {
        return m_entries.value(a).seq < m_entries.value(b).seq;
    });
    for (const QString &path : std::as_const(due)) {
        promote(path, now);
    }

    // Held removals whose grace window ran out without a matching failure:
    // a plain success.
    QStringList expired;
    for (auto it = m_entries.cbegin(); it != m_entries.cend(); ++it) {
        if (it->pendingRemoval && now >= it->removalGraceEndsMs) {
            expired << it.key();
        }
    }
    for (const QString &path : std::as_const(expired)) {
        finishPath(path, false, QString());
    }

    rearm(now);
}

void DownloadProgressController::promote(const QString &path, qint64 now)
{
    auto it = m_entries.find(path);
    if (it == m_entries.end()) {
        return;
    }
    if (m_visibleOrder.size() < MaxVisibleJobs) {
        auto *job = newJob(path);
        job->setFileDescription(QFileInfo(path).fileName());
        job->updateProgress(it->done, it->total, now);
        it->job = job;
        m_visibleOrder << path;
    } else {
        it->inOverflow = true;
        m_overflowOrder << path;
        updateOverflowJob();
    }
}

void DownloadProgressController::finishPath(const QString &path, bool failed, const QString &reason)
{
    auto it = m_entries.find(path);
    if (it == m_entries.end()) {
        return;
    }
    if (DownloadJob *job = it->job) {
        if (failed) {
            job->finishError(reason);
        } else {
            job->finishSuccess();
        }
        m_tracker->unregisterJob(job);
        m_visibleOrder.removeOne(path);
        m_entries.erase(it);
        promoteFromOverflow();
        return;
    }
    if (it->inOverflow) {
        m_overflowOrder.removeOne(path);
        m_entries.erase(it);
        updateOverflowJob();
        return;
    }
    // Never promoted (younger than 2 s, or the switch was off): nothing was shown.
    m_entries.erase(it);
}

void DownloadProgressController::promoteFromOverflow()
{
    if (m_overflowOrder.isEmpty() || m_visibleOrder.size() >= MaxVisibleJobs) {
        return;
    }
    const QString path = m_overflowOrder.takeFirst();
    auto it = m_entries.find(path);
    if (it == m_entries.end()) {
        return;
    }
    it->inOverflow = false;
    const qint64 now = m_clock();
    auto *job = newJob(path);
    job->setFileDescription(QFileInfo(path).fileName());
    job->updateProgress(it->done, it->total, now);
    it->job = job;
    m_visibleOrder << path;
    updateOverflowJob();
}

void DownloadProgressController::updateOverflowJob()
{
    if (m_overflowOrder.isEmpty()) {
        if (m_overflowJob) {
            m_overflowJob->finishSuccess();
            m_tracker->unregisterJob(m_overflowJob);
            m_overflowJob = nullptr;
        }
        return;
    }
    qulonglong doneSum = 0;
    qulonglong totalSum = 0;
    bool totalKnown = true;
    for (const QString &path : std::as_const(m_overflowOrder)) {
        const Entry &entry = m_entries.value(path);
        doneSum += entry.done;
        if (entry.total > 0) {
            totalSum += entry.total;
        } else {
            totalKnown = false;
        }
    }
    if (!m_overflowJob) {
        m_overflowJob = newJob(QStringLiteral("overflow"));
    }
    m_overflowJob->setOverflowDescription(m_overflowOrder.size());
    m_overflowJob->updateProgress(doneSum, totalKnown ? totalSum : 0, m_clock());
}

void DownloadProgressController::rearm(qint64 now)
{
    qint64 next = std::numeric_limits<qint64>::max();
    for (auto it = m_entries.cbegin(); it != m_entries.cend(); ++it) {
        if (it->pendingRemoval) {
            next = qMin(next, it->removalGraceEndsMs);
        } else if (!it->job && !it->inOverflow) {
            next = qMin(next, it->firstSeenMs + PromoteAfterMs);
        }
    }
    if (next == std::numeric_limits<qint64>::max()) {
        m_timer->stop();
        return;
    }
    m_timer->start(int(qBound<qint64>(0, next - now, qMax(PromoteAfterMs, RemovalGraceMs))));
}

void DownloadProgressController::teardownAll()
{
    for (auto it = m_entries.cbegin(); it != m_entries.cend(); ++it) {
        if (it->job) {
            it->job->kill(KJob::Quietly);
            m_tracker->unregisterJob(it->job);
        }
    }
    m_entries.clear();
    m_visibleOrder.clear();
    m_overflowOrder.clear();
    if (m_overflowJob) {
        m_overflowJob->kill(KJob::Quietly);
        m_tracker->unregisterJob(m_overflowJob);
        m_overflowJob = nullptr;
    }
    m_timer->stop();
}

void DownloadProgressController::finishAllWithError(const QString &reason)
{
    // Every job still registered with the tracker, visible or held in the
    // removal grace window, fails now; entries never promoted are simply
    // dropped (nothing was ever shown for them).
    for (auto it = m_entries.cbegin(); it != m_entries.cend(); ++it) {
        if (it->job) {
            it->job->finishError(reason);
            m_tracker->unregisterJob(it->job);
        }
    }
    if (m_overflowJob) {
        m_overflowJob->finishError(reason);
        m_tracker->unregisterJob(m_overflowJob);
        m_overflowJob = nullptr;
    }
    m_entries.clear();
    m_visibleOrder.clear();
    m_overflowOrder.clear();
    m_timer->stop();
}
