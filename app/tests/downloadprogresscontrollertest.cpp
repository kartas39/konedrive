#include "downloadjob.h"
#include "downloadjobtracker.h"
#include "downloadprogresscontroller.h"
#include "downloadprogresssettings.h"
#include "fakedaemon.h"
#include "synccontroller.h"
#include "transfermodel.h"

#include <KJob>

#include <QSignalSpy>
#include <QTest>

#include <memory>

namespace
{
const QString Root = QStringLiteral("/home/u/OneDrive");
}

/// Stands in for KUiServerV2JobTracker: nothing reaches Plasma. Snapshots a
/// finishing job's state at unregisterJob() time, since the job may
/// deleteLater() once the event loop next turns.
class RecordingJobTracker : public DownloadJobTracker
{
public:
    struct Snapshot {
        QString name;
        int error = 0;
        QString errorText;
        qulonglong processed = 0;
        qulonglong total = 0;
        unsigned long percent = 0;
    };

    void registerJob(KJob *job) override
    {
        registered << job;
        // I1: what the job's description already says at the moment it is
        // registered (empty means nothing was emitted yet — the order
        // KUiServerV2JobTracker needs, since it connects to KJob::description
        // only at the end of registerJob()).
        detailAtRegistration << (qobject_cast<DownloadJob *>(job) ? qobject_cast<DownloadJob *>(job)->detailText() : QString());
    }

    void unregisterJob(KJob *job) override
    {
        unregistered << Snapshot{job->objectName(), job->error(), job->errorText(), job->processedAmount(KJob::Bytes), job->totalAmount(KJob::Bytes), job->percent()};
    }

    QList<KJob *> registered;
    QList<Snapshot> unregistered;
    QStringList detailAtRegistration;
};

/// DownloadProgressController: a fake Transfers source
/// (FakeDaemon's FakeSync1, through the real SyncController) and a recording
/// job tracker, with an injected clock and no real waiting for the 2 s
/// promotion or the 5-job cap.
class DownloadProgressControllerTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeDaemon> m_daemon;
    std::unique_ptr<SyncController> m_sync;
    qint64 m_nowMs = 1000000;

    void start()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        QVERIFY(m_daemon->start());
        m_sync = std::make_unique<SyncController>(fake::FirstAccount);
        QTRY_VERIFY(m_sync->serviceAvailable());
    }

private Q_SLOTS:
    void init()
    {
        m_nowMs = 1000000;
    }

    void cleanup()
    {
        m_sync.reset();
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
    }

    void noJobUnderTwoSeconds()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });

        m_daemon->sync->setTransfers({{Root + QStringLiteral("/big.iso"), 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        QCOMPARE(tracker.registered.size(), 0);

        m_nowMs += 1999;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 0);

        // Gone before 2 s: never shown at all, not even as a finish.
        m_daemon->sync->setTransfers({});
        QTRY_COMPARE(m_sync->transfers()->count(), 0);
        QCOMPARE(tracker.registered.size(), 0);
        QCOMPARE(tracker.unregistered.size(), 0);
    }

    void aJobAppearsAtTwoSecondsIsUpdatedAndFinishesWhenTheTransferLeaves()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/big.iso");

        m_daemon->sync->setTransfers({{path, 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        QCOMPARE(tracker.registered.size(), 0);

        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);
        auto *job = qobject_cast<DownloadJob *>(tracker.registered.first());
        QVERIFY(job);
        QCOMPARE(job->objectName(), path);
        QVERIFY2(job->detailText().contains(QStringLiteral("big.iso")), qPrintable(job->detailText()));
        QCOMPARE(job->totalAmount(KJob::Bytes), 5000ULL);
        QCOMPARE(job->processedAmount(KJob::Bytes), 1000ULL);

        m_nowMs += 1000;
        m_daemon->sync->setTransfers({{path, 3000, 5000}});
        QTRY_COMPARE(job->processedAmount(KJob::Bytes), 3000ULL);

        // Leaving Transfers holds it for the grace window (the
        // real daemon's own order), not an immediate finish.
        m_daemon->sync->setTransfers({});
        QTRY_COMPARE(m_sync->transfers()->count(), 0);
        QCOMPARE(tracker.unregistered.size(), 0);

        m_nowMs += DownloadProgressController::RemovalGraceMs;
        controller.checkNow();
        QCOMPARE(tracker.unregistered.size(), 1);
        QCOMPARE(tracker.unregistered.first().name, path);
        QCOMPARE(tracker.unregistered.first().error, int(KJob::NoError));
        QCOMPARE(tracker.unregistered.first().processed, 3000ULL);
    }

    /// With more than one account, a job's title names its account; with one, it stays plain.
    void theTitleNamesTheAccountWhenThereAreSeveral()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        QString name;
        controller.setAccountName([&name] {
            return name;
        });

        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);
        QCOMPARE(qobject_cast<DownloadJob *>(tracker.registered.at(0))->title(), QStringLiteral("Downloading from OneDrive"));

        name = QStringLiteral("Family");
        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}, {Root + QStringLiteral("/b.iso"), 1, 10}});
        QTRY_COMPARE(m_sync->transfers()->count(), 2);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 2);
        QCOMPARE(qobject_cast<DownloadJob *>(tracker.registered.at(1))->title(), QStringLiteral("Downloading from OneDrive — Family"));
    }

    /// I1: a job is registered with Plasma before its title/file name and
    /// its progress go out, the way the overflow job already did — otherwise
    /// KUiServerV2JobTracker, which only starts listening to KJob::description
    /// at the end of registerJob(), never shows them.
    void jobIsRegisteredBeforeItsDescriptionGoesOut()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/big.iso");

        m_daemon->sync->setTransfers({{path, 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);
        QCOMPARE(tracker.detailAtRegistration.first(), QString());

        auto *job = qobject_cast<DownloadJob *>(tracker.registered.first());
        QVERIFY(job);
        QVERIFY2(job->detailText().contains(QStringLiteral("big.iso")), qPrintable(job->detailText()));
    }

    void aFailureFinishesTheJobWithItsReason()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/big.iso");

        m_daemon->sync->setTransfers({{path, 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);

        QSignalSpy arrived(m_sync.get(), &SyncController::activityAdded);
        m_daemon->sync->activity(1758700000, QStringLiteral("failed"), path, QStringLiteral("the connection was reset"));
        QVERIFY(arrived.wait(5000));

        QCOMPARE(tracker.unregistered.size(), 1);
        const RecordingJobTracker::Snapshot &snap = tracker.unregistered.first();
        QCOMPARE(snap.name, path);
        QVERIFY(snap.error != int(KJob::NoError));
        QCOMPARE(snap.errorText, QStringLiteral("the connection was reset"));

        // The transfer leaving afterward does not finish it a second time.
        m_daemon->sync->setTransfers({});
        QTRY_COMPARE(m_sync->transfers()->count(), 0);
        QCOMPARE(tracker.unregistered.size(), 1);
    }

    /// Ruling 4: total 0 (unknown) shows the job without a percentage.
    void anUnknownTotalShowsNoPercentage()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/streaming.bin");

        m_daemon->sync->setTransfers({{path, 500, 0}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);
        auto *job = qobject_cast<DownloadJob *>(tracker.registered.first());
        QVERIFY(job);
        QCOMPARE(job->totalAmount(KJob::Bytes), 0ULL);
        QCOMPARE(job->percent(), 0UL);
        QCOMPARE(job->processedAmount(KJob::Bytes), 500ULL);
    }

    void theCapIsFivePlusAndNMore()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });

        KonedriveTransferList list;
        for (int i = 0; i < 6; ++i) {
            list << KonedriveTransfer{Root + QStringLiteral("/f%1.bin").arg(i), qulonglong(i * 10), 1000};
        }
        m_daemon->sync->setTransfers(list);
        QTRY_COMPARE(m_sync->transfers()->count(), 6);

        m_nowMs += 2000;
        controller.checkNow();

        QCOMPARE(tracker.registered.size(), 6); // 5 files, each its own job, plus 1 summed job
        int visible = 0;
        KJob *overflow = nullptr;
        for (KJob *job : std::as_const(tracker.registered)) {
            if (job->objectName() == QStringLiteral("overflow")) {
                overflow = job;
            } else {
                ++visible;
            }
        }
        QCOMPARE(visible, 5);
        QVERIFY(overflow);
        auto *overflowJob = qobject_cast<DownloadJob *>(overflow);
        QVERIFY(overflowJob);
        QVERIFY2(overflowJob->detailText().contains(QStringLiteral("1 more file")), qPrintable(overflowJob->detailText()));

        // f0.bin leaves: held for the grace window first, then the
        // overflowed f5.bin takes its visible slot, and with nothing left
        // over, the summed job finishes too.
        list.removeFirst();
        m_daemon->sync->setTransfers(list);
        QTRY_COMPARE(m_sync->transfers()->count(), 5);
        QCOMPARE(tracker.unregistered.size(), 0);

        m_nowMs += DownloadProgressController::RemovalGraceMs;
        controller.checkNow();
        QCOMPARE(tracker.unregistered.size(), 2);
        QCOMPARE(tracker.registered.size(), 7);
        bool sawOverflowFinish = false;
        for (const RecordingJobTracker::Snapshot &snap : std::as_const(tracker.unregistered)) {
            if (snap.name == QStringLiteral("overflow")) {
                sawOverflowFinish = true;
            }
        }
        QVERIFY(sawOverflowFinish);
    }

    void theSwitchStopsAnyJobFromAppearing()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressSettings settings;
        settings.setEnabled(false);
        DownloadProgressController controller(m_sync.get(), &tracker, &settings, [this] {
            return m_nowMs;
        });

        m_daemon->sync->setTransfers({{Root + QStringLiteral("/big.iso"), 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);

        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 0);

        // Turning it on tracks from now: the transfer needs its own 2 s
        // again before it shows.
        settings.setEnabled(true);
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 0);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);
    }

    /// The real daemon drops the transfer from Transfers before
    /// the ActivityAdded(failed) for it goes out (crates/konedrived/src/sync/mod.rs
    /// ~202, then the helper round trip). A removal must not finish the job
    /// at once: it is held for a 1.5 s grace window, and a matching failure
    /// inside that window still fails it.
    void removalThenFailureWithinTheGraceWindowGivesAnError()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/big.iso");

        m_daemon->sync->setTransfers({{path, 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);

        // The transfer leaves Transfers first, as the real daemon does.
        m_daemon->sync->setTransfers({});
        QTRY_COMPARE(m_sync->transfers()->count(), 0);
        QCOMPARE(tracker.unregistered.size(), 0); // held, not finished yet

        m_nowMs += 500; // inside the 1.5 s window
        QSignalSpy arrived(m_sync.get(), &SyncController::activityAdded);
        m_daemon->sync->activity(1758700000, QStringLiteral("failed"), path, QStringLiteral("the connection was reset"));
        QVERIFY(arrived.wait(5000));

        QCOMPARE(tracker.unregistered.size(), 1);
        const RecordingJobTracker::Snapshot &snap = tracker.unregistered.first();
        QCOMPARE(snap.name, path);
        QVERIFY(snap.error != int(KJob::NoError));
        QCOMPARE(snap.errorText, QStringLiteral("the connection was reset"));
    }

    /// Same removal-first order, but nothing follows: the job finishes as a
    /// plain success once the 1.5 s grace window ends.
    void removalThenNothingGivesSuccessAfterTheGraceWindow()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/big.iso");

        m_daemon->sync->setTransfers({{path, 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 1);

        m_daemon->sync->setTransfers({});
        QTRY_COMPARE(m_sync->transfers()->count(), 0);
        QCOMPARE(tracker.unregistered.size(), 0);

        m_nowMs += 1499; // just short of the grace window
        controller.checkNow();
        QCOMPARE(tracker.unregistered.size(), 0);

        m_nowMs += 1; // exactly at the grace window
        controller.checkNow();
        QCOMPARE(tracker.unregistered.size(), 1);
        QCOMPARE(tracker.unregistered.first().error, int(KJob::NoError));
    }

    /// A daemon restart (serviceAvailable false) must not freeze visible
    /// jobs and later quietly "finish" them as success: every visible and
    /// overflow job ends with an error at once.
    void aDaemonRestartFinishesVisibleAndOverflowJobsWithAnError()
    {
        start();
        RecordingJobTracker tracker;
        DownloadProgressController controller(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });

        KonedriveTransferList list;
        for (int i = 0; i < 6; ++i) {
            list << KonedriveTransfer{Root + QStringLiteral("/f%1.bin").arg(i), qulonglong(i * 10), 1000};
        }
        m_daemon->sync->setTransfers(list);
        QTRY_COMPARE(m_sync->transfers()->count(), 6);
        m_nowMs += 2000;
        controller.checkNow();
        QCOMPARE(tracker.registered.size(), 6); // 5 visible + 1 summed overflow job

        m_daemon->stop();
        QTRY_VERIFY(!m_sync->serviceAvailable());

        QCOMPARE(tracker.unregistered.size(), 6);
        for (const RecordingJobTracker::Snapshot &snap : std::as_const(tracker.unregistered)) {
            QVERIFY(snap.error != int(KJob::NoError));
            QCOMPARE(snap.errorText, QStringLiteral("the KOneDrive service stopped"));
        }
    }

    void theDestructorUnregistersEveryJob()
    {
        start();
        RecordingJobTracker tracker;
        auto controller = std::make_unique<DownloadProgressController>(m_sync.get(), &tracker, nullptr, [this] {
            return m_nowMs;
        });
        const QString path = Root + QStringLiteral("/big.iso");

        m_daemon->sync->setTransfers({{path, 1000, 5000}});
        QTRY_COMPARE(m_sync->transfers()->count(), 1);
        m_nowMs += 2000;
        controller->checkNow();
        QCOMPARE(tracker.registered.size(), 1);
        QCOMPARE(tracker.unregistered.size(), 0);

        controller.reset();
        QCOMPARE(tracker.unregistered.size(), 1);
    }
};

QTEST_GUILESS_MAIN(DownloadProgressControllerTest)

#include "downloadprogresscontrollertest.moc"
