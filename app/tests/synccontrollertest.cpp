#include "activitymodel.h"
#include "conflictmodel.h"
#include "fakedaemon.h"
#include "synccontroller.h"
#include "transfermodel.h"

#include <QAbstractItemModelTester>
#include <QSignalSpy>
#include <QTest>

#include <memory>

class SyncControllerTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeDaemon> m_daemon;
    FakeSync *m_fake = nullptr;

    void startFake()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        m_fake = m_daemon->sync;
        QVERIFY(m_daemon->start());
    }

    static QString text(const QAbstractItemModel *model, int row, int role)
    {
        return model->data(model->index(row, 0), role).toString();
    }

private Q_SLOTS:
    void cleanup()
    {
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
        m_fake = nullptr;
    }

    void followsTheFolderAndItsCounters()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        m_fake->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/OneDrive")},
                             {QStringLiteral("State"), QStringLiteral("listing")},
                             {QStringLiteral("Source"), QStringLiteral("onedrive")},
                             {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(12480)}});
        QTRY_COMPARE(controller.itemsListed(), 12480ULL);
        QCOMPARE(controller.rootState(), QStringLiteral("listing"));
        QCOMPARE(controller.rootSource(), QStringLiteral("onedrive"));
    }

    /// Each of the folder's interfaces is read and followed on its own: a change of
    /// LocalScan's State is the scan's, never the folder's State of the same name, and a
    /// change of Transfers and of Conflicts reaches the window too.
    void followsEveryInterfaceOfTheFolder()
    {
        startFake();
        m_fake->scan->set({{QStringLiteral("Reason"), QStringLiteral("overflow")}});
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.scanReason(), QStringLiteral("overflow")); // from GetAll
        QCOMPARE(controller.poolSize(), 16u);

        m_fake->folder->set({{QStringLiteral("State"), QStringLiteral("ready")}});
        m_fake->scan->set({{QStringLiteral("State"), QStringLiteral("running")}, {QStringLiteral("Files"), QVariant::fromValue<qulonglong>(7)}});
        QTRY_COMPARE(controller.scanState(), QStringLiteral("running"));
        QCOMPARE(controller.scanFiles(), 7ULL);
        QCOMPARE(controller.rootState(), QStringLiteral("ready"));

        m_fake->transfers->set({{QStringLiteral("PoolSize"), QVariant::fromValue<uint>(8)}, {QStringLiteral("LargeStreams"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(controller.poolSize(), 8u);
        QCOMPARE(controller.largeTransfers(), 2u);

        // The pool line's numbers (issue #50): the slots in use (above the size here: an
        // open's reserve), the large files, their streams and the streams' limit.
        m_fake->transfers->set({{QStringLiteral("PoolInUse"), QVariant::fromValue<uint>(10)},
                                {QStringLiteral("LargeFiles"), QVariant::fromValue<uint>(1)},
                                {QStringLiteral("LargeStreams"), QVariant::fromValue<uint>(4)},
                                {QStringLiteral("LargeStreamLimit"), QVariant::fromValue<uint>(4)}});
        QTRY_COMPARE(controller.poolInUse(), 10u);
        QCOMPARE(controller.largeFiles(), 1u);
        QCOMPARE(controller.largeTransfers(), 4u);
        QCOMPARE(controller.largeLimit(), 4u);
        m_fake->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(3)}});
        QTRY_COMPARE(controller.conflictCount(), 3u);
    }

    /// What waits to go up, and the controls over it: the counts, pause and
    /// resume, the ignore list, the mass-delete guard's two answers, and what
    /// is kept back — by reason, with the files of a reason only when shown.
    void theOutboxAndItsControls()
    {
        startFake();
        const QString root = QStringLiteral("/home/u/OneDrive");
        m_fake->outboxRows = {{1, QStringLiteral("update"), root + QStringLiteral("/a.odt"), QStringLiteral("running"), 5, 10, QString(), 0},
                              {2, QStringLiteral("create"), root + QStringLiteral("/a:b"), QStringLiteral("blocked"), 0, 3, QStringLiteral("name-characters"), 0}};
        m_fake->holdDeletes(root + QStringLiteral("/old"), 1);
        m_fake->keptBack = {{QStringLiteral("one-action"), QStringLiteral("quota-exceeded"), 5000, 7ULL << 30},
                            {QStringLiteral("per-file"), QStringLiteral("name-characters"), 1, 3},
                            {QStringLiteral("never"), QStringLiteral("symlink"), 1, 0}};
        m_fake->keptBackFiles.insert(QStringLiteral("name-characters"), {{root + QStringLiteral("/a:b"), QStringLiteral("name-characters")}});
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.machineName(), QStringLiteral("fedora"));
        QCOMPARE(controller.ignorePatterns(), (QStringList{QStringLiteral("*.tmp"), QStringLiteral("~*")}));
        QCOMPARE(controller.heldCount(), 1u);

        m_fake->queue->set({{QStringLiteral("PendingCount"), QVariant::fromValue<uint>(1)},
                            {QStringLiteral("PendingBytes"), QVariant::fromValue<qulonglong>(10)},
                            {QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(1)}});
        QTRY_COMPARE(controller.blockedCount(), 1u);
        QCOMPARE(controller.pendingCount(), 1u);
        QCOMPARE(controller.pendingBytes(), 10ULL);

        controller.pause(7200);
        QTRY_VERIFY(controller.paused());
        QCOMPARE(controller.pausedUntil(), m_fake->pauseNow + 7200);
        controller.resume();
        QTRY_VERIFY(!controller.paused());

        controller.addIgnorePattern(QStringLiteral(" *.bak "));
        QTRY_COMPARE(controller.ignorePatterns(), (QStringList{QStringLiteral("*.tmp"), QStringLiteral("~*"), QStringLiteral("*.bak")}));
        controller.removeIgnorePattern(QStringLiteral("~*"));
        QTRY_COMPARE(controller.ignorePatterns(), (QStringList{QStringLiteral("*.tmp"), QStringLiteral("*.bak")}));
        controller.addIgnorePattern(QStringLiteral("a/b"));
        QTRY_VERIFY(controller.actionError().contains(QStringLiteral("not a pattern")));

        controller.restoreDeletes();
        QTRY_COMPARE(controller.heldCount(), 0u);
        QVERIFY(m_fake->calls.contains(QStringLiteral("RestoreDeletes")));

        controller.loadNotUploaded();
        QTRY_VERIFY(controller.notUploadedKnown());
        QCOMPARE(controller.notUploadedSummary().size(), 3);
        QCOMPARE(controller.notUploadedSummary().at(2).toMap().value(QStringLiteral("why")).toString(), QStringLiteral("A symbolic link: never uploaded."));
        QCOMPARE(controller.blockedBytes(), (7ULL << 30) + 3);
        QVERIFY(controller.notUploadedFiles().isEmpty());
        controller.setNotUploadedFilesShown(QStringLiteral("name-characters"), true);
        QTRY_VERIFY(controller.notUploadedFiles().contains(QStringLiteral("name-characters")));
        const QVariantMap files = controller.notUploadedFiles().value(QStringLiteral("name-characters")).toMap();
        QCOMPARE(files.value(QStringLiteral("total")).toUInt(), 1u);
        QVERIFY(files.value(QStringLiteral("items")).toList().first().toMap().value(QStringLiteral("why")).toString().startsWith(QStringLiteral("A name OneDrive refuses")));
        QVERIFY(m_fake->calls.contains(QStringLiteral("NotUploadedFiles:name-characters:20")));
        // Asked for again and again: at most once a second, the last not dropped.
        const auto asked = m_fake->calls.count(QStringLiteral("NotUploadedSummary"));
        for (int i = 0; i < 5; ++i) {
            controller.loadNotUploaded();
        }
        QTest::qWait(1300);
        const auto more = m_fake->calls.count(QStringLiteral("NotUploadedSummary")) - asked;
        QVERIFY2(more >= 1 && more <= 2, qPrintable(QString::number(more)));
        // The window never asks for every row.
        QVERIFY(!m_fake->calls.contains(QStringLiteral("Changes")));
        QVERIFY(!m_fake->calls.contains(QStringLiteral("NotUploaded")));
    }

    /// M8: nothing will update Transfers again once the daemon is gone, so a
    /// stale row must not linger looking like a download still going.
    void theServiceGoingAwayClearsTransfers()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        m_fake->setTransfers({{QStringLiteral("/home/u/OneDrive/big.iso"), 1, 10}});
        QTRY_COMPARE(controller.transfers()->count(), 1);
        m_daemon->stop();
        QTRY_VERIFY(!controller.serviceAvailable());
        QCOMPARE(controller.transfers()->count(), 0);
    }

    void choosingAFolderRegistersIt()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.chooseFolder(QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QTRY_COMPARE(controller.rootPath(), QStringLiteral("/home/u/OneDrive"));
        QVERIFY(m_fake->calls.contains(QStringLiteral("Register:/home/u/OneDrive")));
    }

    /// Without the helper the window asks before registering a folder whose
    /// placeholders nothing fills on open (part 1's A5). The
    /// window never registers a folder
    /// without interception — an unhydrated file would read as zeros for
    /// good. The prompt's only way forward is "Try Again", which retries
    /// RegisterRoot.
    void withoutTheHelperTheUserWaitsOrRetries()
    {
        startFake();
        m_fake->helperMissing = true;
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.chooseFolder(QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QTRY_COMPARE(controller.pendingFolder(), QStringLiteral("/home/u/OneDrive"));
        QCOMPARE(m_fake->calls.count(QStringLiteral("Register:/home/u/OneDrive")), 1);

        // Retrying while the helper is still missing changes nothing.
        controller.retryRegistration();
        QTRY_COMPARE(m_fake->calls.count(QStringLiteral("Register:/home/u/OneDrive")), 2);
        QCOMPARE(controller.pendingFolder(), QStringLiteral("/home/u/OneDrive"));

        // The helper connects while the prompt is up; "Try Again" retries
        // RegisterRoot and this time it takes.
        m_fake->helperMissing = false;
        controller.retryRegistration();
        QTRY_COMPARE(controller.pendingFolder(), QString());
        QCOMPARE(controller.rootState(), QStringLiteral("listing"));
        QCOMPARE(m_fake->calls.count(QStringLiteral("Register:/home/u/OneDrive")), 3);
        QVERIFY(!m_fake->calls.contains(QStringLiteral("RegisterWithoutInterception:/home/u/OneDrive")));
    }

    void listsWhatIsSkippedWithWhy()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.loadSkipped();
        QTRY_COMPARE(controller.skipped().size(), 1);
        const QVariantMap item = controller.skipped().first().toMap();
        QCOMPARE(item.value(QStringLiteral("path")).toString(), QStringLiteral("/home/u/OneDrive/Personal Vault"));
        QVERIFY(item.value(QStringLiteral("why")).toString().contains(QStringLiteral("locked separately")));
    }

    /// M7: loadSkipped() is an incidental background reload (the Skipped
    /// page's own onVisibleChanged/onSyncChanged), so it must never clear an
    /// actionError a real action just set — only a new action gets to.
    void loadSkippedNeverClearsAnActionError()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.dismissConflict(QStringLiteral("/no/such/rescue"));
        QTRY_VERIFY(!controller.actionError().isEmpty());
        controller.loadSkipped();
        QTRY_COMPARE(controller.skipped().size(), 1);
        QVERIFY(!controller.actionError().isEmpty());
    }

    void refreshAndForgetCallTheDaemon()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.refresh();
        controller.forget();
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("Unregister")));
        QVERIFY(m_fake->calls.contains(QStringLiteral("Refresh")));
    }

    /// The scalar properties reach the window.
    void followsLastCheckedLocalSpaceAndConflictCount()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        m_fake->folder->set({{QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(1758700000)},
                             {QStringLiteral("LocalBytes"), QVariant::fromValue<qulonglong>(1288490188)}});
        m_fake->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(controller.lastChecked(), 1758700000LL);
        QCOMPARE(controller.localBytes(), 1288490188ULL);
        QCOMPARE(controller.conflictCount(), 2U);
    }

    /// PinnedCount (dbus/org.konedrive.Folder.xml) reaches the window, both
    /// from GetAll at start and from a PropertiesChanged that follows.
    void followsPinnedCount()
    {
        startFake();
        m_fake->folder->set({{QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(3)}});
        SyncController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.pinnedCount(), 3U);

        m_fake->folder->set({{QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(5)}});
        QTRY_COMPARE(controller.pinnedCount(), 5U);
    }

    /// Transfers (a(stt)) arrives as a D-Bus structure inside PropertiesChanged
    /// and in GetAll; both land in the model, and a download that goes on is
    /// updated in place.
    void transfersFollowTheProperty()
    {
        startFake();
        m_fake->setTransfers({{QStringLiteral("/home/u/OneDrive/a.iso"), 10, 100}});
        SyncController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.transfers()->count(), 1); // from GetAll
        QCOMPARE(text(controller.transfers(), 0, TransferModel::PathRole), QStringLiteral("/home/u/OneDrive/a.iso"));

        QAbstractItemModelTester tester(controller.transfers(), QAbstractItemModelTester::FailureReportingMode::QtTest);
        QSignalSpy reset(controller.transfers(), &QAbstractItemModel::modelReset);
        m_fake->setTransfers({{QStringLiteral("/home/u/OneDrive/a.iso"), 50, 100}, {QStringLiteral("/home/u/OneDrive/b.txt"), 0, 7}});
        QTRY_COMPARE(controller.transfers()->count(), 2);
        const QModelIndex a = controller.transfers()->index(0, 0);
        QCOMPARE(a.data(TransferModel::FractionRole).toDouble(), 0.5);

        m_fake->setTransfers({});
        QTRY_COMPARE(controller.transfers()->count(), 0);
        QCOMPARE(reset.count(), 0);
    }

    /// The "Recent" list: Recent(50) at start, then each ActivityLog.Added
    /// on top, newest first.
    void recentActivityThenLiveEventsNewestFirst()
    {
        startFake();
        m_fake->log = {{200, QStringLiteral("downloaded"), QStringLiteral("/home/u/OneDrive/b.txt"), QStringLiteral("7 bytes")},
                       {100, QStringLiteral("listed"), QStringLiteral("/home/u/OneDrive"), QStringLiteral("12 items")}};
        SyncController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.activity()->count(), 2);
        QVERIFY(m_fake->calls.contains(QStringLiteral("Recent:50")));
        QCOMPARE(text(controller.activity(), 0, ActivityModel::PathRole), QStringLiteral("/home/u/OneDrive/b.txt"));

        QSignalSpy added(&controller, &SyncController::activityAdded);
        m_fake->activity(300, QStringLiteral("freed"), QStringLiteral("/home/u/OneDrive/c.bin"), QString());
        QVERIFY(added.wait(5000));
        QCOMPARE(added.first().at(1).toString(), QStringLiteral("freed"));
        QCOMPARE(controller.activity()->count(), 3);
        QCOMPARE(text(controller.activity(), 0, ActivityModel::PathRole), QStringLiteral("/home/u/OneDrive/c.bin"));
        QCOMPARE(text(controller.activity(), 2, ActivityModel::KindRole), QStringLiteral("listed"));
    }

    /// Dismissing a conflict asks for the list again, whether or not a
    /// ConflictCount change follows.
    void dismissingAConflictRefreshesTheList()
    {
        startFake();
        m_fake->conflictList = {{200, QStringLiteral("/home/u/OneDrive/doc.odt"), QStringLiteral("/home/u/.local/share/konedrive/rescued/2/doc.odt")},
                                {100, QStringLiteral("/home/u/OneDrive/a.txt"), QStringLiteral("/home/u/.local/share/konedrive/rescued/1/a.txt")}};
        m_fake->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(2)}});
        SyncController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.conflicts()->count(), 2);

        controller.dismissConflict(QStringLiteral("/home/u/.local/share/konedrive/rescued/2/doc.odt"));
        QTRY_COMPARE(controller.conflicts()->count(), 1);
        QVERIFY(m_fake->calls.contains(QStringLiteral("Dismiss:/home/u/.local/share/konedrive/rescued/2/doc.odt")));
        QCOMPARE(text(controller.conflicts(), 0, ConflictModel::OriginalRole), QStringLiteral("/home/u/OneDrive/a.txt"));
        QCOMPARE(controller.actionError(), QString());
    }

    /// A new conflict raises ConflictCount; the list follows.
    void aConflictCountChangeReloadsTheList()
    {
        startFake();
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.conflicts()->count(), 0);
        m_fake->conflictList = {{100, QStringLiteral("/home/u/OneDrive/a.txt"), QStringLiteral("/r/1/a.txt")}};
        m_fake->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(1)}});
        QTRY_COMPARE(controller.conflicts()->count(), 1);
    }

    /// Freeing up a large folder can take longer than D-Bus's 25 s default:
    /// the call waits as long as it takes, busy all the while (review B1).
    /// Ordinary calls here give up after 50 ms, so a Refresh sent after it
    /// and never answered shows that time has passed.
    void freeUpSpaceWaitsAsLongAsItTakes()
    {
        startFake();
        m_fake->holdFreeUp = true;
        m_fake->holdRefresh = true;
        m_fake->freedFiles = 2;
        m_fake->freedBytes = 8192;
        SyncController controller(fake::FirstAccount);
        controller.setCallTimeout(50);
        QTRY_VERIFY(controller.serviceAvailable());

        controller.freeUpSpace();
        QVERIFY(controller.freeingUp());
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("FreeUpSpace")));
        controller.refresh();
        QTRY_VERIFY(!controller.actionError().isEmpty()); // the Refresh gave up
        QCoreApplication::processEvents();
        QVERIFY(controller.freeingUp());
        QCOMPARE(controller.freeUpResult(), QString());

        m_fake->finishFreeUp();
        QTRY_VERIFY(!controller.freeingUp());
        QVERIFY2(controller.freeUpResult().startsWith(QStringLiteral("Freed 2 files")), qPrintable(controller.freeUpResult()));
    }

    /// Live events that arrive while ActivityLog.Recent() is on its way are kept
    /// when its answer lands, once each (review B3).
    void liveEventsDuringALoadAreKeptOnce()
    {
        startFake();
        m_fake->log = {{100, QStringLiteral("listed"), QStringLiteral("/home/u/OneDrive"), QStringLiteral("12 items")}};
        m_fake->holdActivity = true;
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("Recent:50")));

        QSignalSpy added(&controller, &SyncController::activityAdded);
        // Signalled before the daemon stored it: the answer will lack it.
        m_fake->signalOnly(300, QStringLiteral("downloaded"), QStringLiteral("/home/u/OneDrive/early.txt"), QStringLiteral("1 KiB"));
        // Stored, then signalled: the answer has it as well.
        m_fake->activity(200, QStringLiteral("freed"), QStringLiteral("/home/u/OneDrive/both.txt"), QString());
        QTRY_COMPARE(added.count(), 2);
        QCOMPARE(controller.activity()->count(), 2);

        m_fake->releaseActivity();
        QTRY_COMPARE(controller.activity()->count(), 3);
        QCoreApplication::processEvents();
        QCOMPARE(controller.activity()->count(), 3);
        QCOMPARE(text(controller.activity(), 0, ActivityModel::PathRole), QStringLiteral("/home/u/OneDrive/early.txt"));
        QCOMPARE(text(controller.activity(), 1, ActivityModel::PathRole), QStringLiteral("/home/u/OneDrive/both.txt"));
        QCOMPARE(text(controller.activity(), 2, ActivityModel::KindRole), QStringLiteral("listed"));
    }

    /// M9: the daemon stores an event before it signals it, so a
    /// ActivityLog.Recent() reply already on its way can already include it; the
    /// ActivityLog.Added that follows for that same event must not list it twice.
    void aLiveEventAlreadyInTheReplyIsNotListedTwice()
    {
        startFake();
        m_fake->holdActivity = true;
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("Recent:50")));

        // Stored (as the daemon always does before signalling) and already in
        // what the held reply will answer with.
        m_fake->log.prepend({200, QStringLiteral("freed"), QStringLiteral("/home/u/OneDrive/both.txt"), QString()});
        m_fake->releaseActivity();
        QTRY_COMPARE(controller.activity()->count(), 1);

        // The signal for that same event, arriving only now.
        QSignalSpy added(&controller, &SyncController::activityAdded);
        m_fake->signalOnly(200, QStringLiteral("freed"), QStringLiteral("/home/u/OneDrive/both.txt"), QString());
        QVERIFY(added.wait(5000));
        QCoreApplication::processEvents();
        QCOMPARE(controller.activity()->count(), 1);
    }

    void freeUpSpaceSaysWhatItDid()
    {
        startFake();
        m_fake->freedFiles = 3;
        m_fake->freedBytes = 3 * 1024 * 1024;
        m_fake->busyFiles = 1;
        SyncController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.freeUpSpace();
        QTRY_VERIFY(!controller.freeUpResult().isEmpty());
        QVERIFY2(controller.freeUpResult().contains(QStringLiteral("Freed 3 files")), qPrintable(controller.freeUpResult()));
        QVERIFY2(controller.freeUpResult().contains(QStringLiteral("1 file was in use")), qPrintable(controller.freeUpResult()));
        QVERIFY(!controller.freeingUp());
    }
};

QTEST_GUILESS_MAIN(SyncControllerTest)

#include "synccontrollertest.moc"
