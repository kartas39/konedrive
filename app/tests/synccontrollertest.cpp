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
    FakeSync1 *m_fake = nullptr;

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
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        m_fake->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/OneDrive")},
                     {QStringLiteral("RootState"), QStringLiteral("listing")},
                     {QStringLiteral("RootSource"), QStringLiteral("onedrive")},
                     {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(12480)}});
        QTRY_COMPARE(controller.itemsListed(), 12480ULL);
        QCOMPARE(controller.rootState(), QStringLiteral("listing"));
        QCOMPARE(controller.rootSource(), QStringLiteral("onedrive"));
    }

    /// M8: nothing will update Transfers again once the daemon is gone, so a
    /// stale row must not linger looking like a download still going.
    void theServiceGoingAwayClearsTransfers()
    {
        startFake();
        SyncController controller;
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
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.chooseFolder(QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QTRY_COMPARE(controller.rootPath(), QStringLiteral("/home/u/OneDrive"));
        QVERIFY(m_fake->calls.contains(QStringLiteral("RegisterRoot:/home/u/OneDrive")));
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
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.chooseFolder(QUrl::fromLocalFile(QStringLiteral("/home/u/OneDrive")));
        QTRY_COMPARE(controller.pendingFolder(), QStringLiteral("/home/u/OneDrive"));
        QCOMPARE(m_fake->calls.count(QStringLiteral("RegisterRoot:/home/u/OneDrive")), 1);

        // Retrying while the helper is still missing changes nothing.
        controller.retryRegistration();
        QTRY_COMPARE(m_fake->calls.count(QStringLiteral("RegisterRoot:/home/u/OneDrive")), 2);
        QCOMPARE(controller.pendingFolder(), QStringLiteral("/home/u/OneDrive"));

        // The helper connects while the prompt is up; "Try Again" retries
        // RegisterRoot and this time it takes.
        m_fake->helperMissing = false;
        controller.retryRegistration();
        QTRY_COMPARE(controller.pendingFolder(), QString());
        QCOMPARE(controller.rootState(), QStringLiteral("listing"));
        QCOMPARE(m_fake->calls.count(QStringLiteral("RegisterRoot:/home/u/OneDrive")), 3);
        QVERIFY(!m_fake->calls.contains(QStringLiteral("RegisterRootWithoutInterception:/home/u/OneDrive")));
    }

    void listsWhatIsSkippedWithWhy()
    {
        startFake();
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.loadSkipped();
        QTRY_COMPARE(controller.skipped().size(), 1);
        const QVariantMap item = controller.skipped().first().toMap();
        QCOMPARE(item.value(QStringLiteral("path")).toString(), QStringLiteral("/home/u/OneDrive/Personal Vault"));
        QVERIFY(item.value(QStringLiteral("why")).toString().contains(QStringLiteral("locked separately")));
    }

    /// HelperState (dbus/org.konedrive.Sync1.xml) reaches the window and
    /// drives helperTrouble (what StatusPage's helper card and the tray key
    /// off) and helperInstruction (what that card and the NoHelper prompt say
    /// to do about it).
    void helperStateDrivesTroubleAndItsInstruction()
    {
        startFake();
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.helperState(), QStringLiteral("connected"));
        QVERIFY(!controller.helperTrouble());
        QCOMPARE(controller.helperInstruction(), QString());

        m_fake->set({{QStringLiteral("HelperState"), QStringLiteral("not-installed")}});
        QTRY_VERIFY(controller.helperTrouble());
        QVERIFY2(controller.helperInstruction().contains(QStringLiteral("install-helper.sh")), qPrintable(controller.helperInstruction()));

        m_fake->set({{QStringLiteral("HelperState"), QStringLiteral("stopped")}});
        QTRY_VERIFY(controller.helperInstruction().contains(QStringLiteral("systemctl start")));

        m_fake->set({{QStringLiteral("HelperState"), QStringLiteral("failed")}});
        QTRY_VERIFY(controller.helperInstruction().contains(QStringLiteral("systemctl status")));

        m_fake->set({{QStringLiteral("HelperState"), QStringLiteral("unknown")}});
        QTRY_VERIFY(controller.helperTrouble());
        QVERIFY(!controller.helperInstruction().isEmpty());

        m_fake->set({{QStringLiteral("HelperState"), QStringLiteral("connected")}});
        QTRY_VERIFY(!controller.helperTrouble());
        QCOMPARE(controller.helperInstruction(), QString());
    }

    /// M7: loadSkipped() is an incidental background reload (the Skipped
    /// page's own onVisibleChanged/onSyncChanged), so it must never clear an
    /// actionError a real action just set — only a new action gets to.
    void loadSkippedNeverClearsAnActionError()
    {
        startFake();
        SyncController controller;
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
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.refresh();
        controller.forget();
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("UnregisterRoot")));
        QVERIFY(m_fake->calls.contains(QStringLiteral("Refresh")));
    }

    /// The scalar properties reach the window.
    void followsLastCheckedLocalSpaceAndConflictCount()
    {
        startFake();
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        m_fake->set({{QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(1758700000)},
                     {QStringLiteral("LocalBytes"), QVariant::fromValue<qulonglong>(1288490188)},
                     {QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(controller.lastChecked(), 1758700000LL);
        QCOMPARE(controller.localBytes(), 1288490188ULL);
        QCOMPARE(controller.conflictCount(), 2U);
    }

    /// PinnedCount (dbus/org.konedrive.Sync1.xml) reaches the window, both
    /// from GetAll at start and from a PropertiesChanged that follows.
    void followsPinnedCount()
    {
        startFake();
        m_fake->set({{QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(3)}});
        SyncController controller;
        QTRY_COMPARE(controller.pinnedCount(), 3U);

        m_fake->set({{QStringLiteral("PinnedCount"), QVariant::fromValue<uint>(5)}});
        QTRY_COMPARE(controller.pinnedCount(), 5U);
    }

    /// Transfers (a(stt)) arrives as a D-Bus structure inside PropertiesChanged
    /// and in GetAll; both land in the model, and a download that goes on is
    /// updated in place.
    void transfersFollowTheProperty()
    {
        startFake();
        m_fake->setTransfers({{QStringLiteral("/home/u/OneDrive/a.iso"), 10, 100}});
        SyncController controller;
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

    /// The "Recent" list: RecentActivity(50) at start, then each ActivityAdded
    /// on top, newest first.
    void recentActivityThenLiveEventsNewestFirst()
    {
        startFake();
        m_fake->log = {{200, QStringLiteral("downloaded"), QStringLiteral("/home/u/OneDrive/b.txt"), QStringLiteral("7 bytes")},
                       {100, QStringLiteral("listed"), QStringLiteral("/home/u/OneDrive"), QStringLiteral("12 items")}};
        SyncController controller;
        QTRY_COMPARE(controller.activity()->count(), 2);
        QVERIFY(m_fake->calls.contains(QStringLiteral("RecentActivity:50")));
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
        m_fake->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(2)}});
        SyncController controller;
        QTRY_COMPARE(controller.conflicts()->count(), 2);

        controller.dismissConflict(QStringLiteral("/home/u/.local/share/konedrive/rescued/2/doc.odt"));
        QTRY_COMPARE(controller.conflicts()->count(), 1);
        QVERIFY(m_fake->calls.contains(QStringLiteral("DismissConflict:/home/u/.local/share/konedrive/rescued/2/doc.odt")));
        QCOMPARE(text(controller.conflicts(), 0, ConflictModel::OriginalRole), QStringLiteral("/home/u/OneDrive/a.txt"));
        QCOMPARE(controller.actionError(), QString());
    }

    /// A new conflict raises ConflictCount; the list follows.
    void aConflictCountChangeReloadsTheList()
    {
        startFake();
        SyncController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.conflicts()->count(), 0);
        m_fake->conflictList = {{100, QStringLiteral("/home/u/OneDrive/a.txt"), QStringLiteral("/r/1/a.txt")}};
        m_fake->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(1)}});
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
        SyncController controller;
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

    /// Live events that arrive while RecentActivity() is on its way are kept
    /// when its answer lands, once each (review B3).
    void liveEventsDuringALoadAreKeptOnce()
    {
        startFake();
        m_fake->log = {{100, QStringLiteral("listed"), QStringLiteral("/home/u/OneDrive"), QStringLiteral("12 items")}};
        m_fake->holdActivity = true;
        SyncController controller;
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("RecentActivity:50")));

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
    /// RecentActivity() reply already on its way can already include it; the
    /// ActivityAdded that follows for that same event must not list it twice.
    void aLiveEventAlreadyInTheReplyIsNotListedTwice()
    {
        startFake();
        m_fake->holdActivity = true;
        SyncController controller;
        QTRY_VERIFY(m_fake->calls.contains(QStringLiteral("RecentActivity:50")));

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
        SyncController controller;
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
