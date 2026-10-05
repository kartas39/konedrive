#include "accountcontroller.h"
#include "accountsmodel.h"
#include "accountstatus.h"
#include "appstatus.h"
#include "daemoncontroller.h"
#include "fakedaemon.h"
#include "synccontroller.h"
#include "trayicon.h"

#include <KStatusNotifierItem>

#include <QAction>
#include <QMenu>
#include <QSignalSpy>
#include <QTest>
#include <QTimer>
#include <QWindow>

#include <memory>

namespace
{
const QString Root = QStringLiteral("/home/u/OneDrive");
constexpr qint64 Now = 1758700000;
}

/// An account's status line and state, the tray's worst state across
/// accounts and its tooltip, and the tray itself, driven by a fake daemon on
/// the private bus and a fake clock. The state is the daemon's (Folder.Overall):
/// the fake decides nothing, each test says what the daemon decided.
class AppStatusTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeDaemon> m_daemon;
    qint64 m_now = Now;
    std::unique_ptr<DaemonController> m_manager;
    std::unique_ptr<AccountsModel> m_accounts;
    std::unique_ptr<AppStatus> m_app;
    /// The first account's, owned by m_accounts.
    AccountStatus *m_status = nullptr;
    SyncController *m_sync = nullptr;

    /// Follows the (started) fake daemon through the model, as the app does.
    void follow(int accounts = 1)
    {
        m_manager = std::make_unique<DaemonController>();
        m_accounts = std::make_unique<AccountsModel>(m_manager.get(), [this] {
            return m_now;
        });
        m_app = std::make_unique<AppStatus>(m_accounts.get());
        QTRY_COMPARE(m_accounts->count(), accounts);
        if (accounts > 0) {
            m_status = m_accounts->at(0)->status();
            m_sync = m_accounts->at(0)->sync();
        }
    }

    /// A second account, "Family", signed in with a ready folder checked 90 s ago.
    FakeAccountObject *addFamily()
    {
        FakeAccountObject *family = m_daemon->addAccount(QStringLiteral("Family"));
        family->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        family->sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/Family")},
                                   {QStringLiteral("State"), QStringLiteral("ready")},
                                   {QStringLiteral("Source"), QStringLiteral("onedrive")},
                                   {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(Now - 90)}});
        family->sync->folder->decide("ok", "up-to-date");
        return family;
    }

    /// Signed in, a OneDrive folder that is ready, checked 20 s ago: "ok".
    void startSynced()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        m_daemon->sync->folder->set({{QStringLiteral("Path"), Root},
                                     {QStringLiteral("State"), QStringLiteral("ready")},
                                     {QStringLiteral("Source"), QStringLiteral("onedrive")},
                                     {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(Now - 20)}});
        decide("ok", "up-to-date");
        QVERIFY(m_daemon->start());
        follow();
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
    }

    /// What the daemon decided of the first account, with what else changes in the same message.
    void decide(const char *state, const char *reason, const QVariantMap &with = {})
    {
        m_daemon->sync->folder->decide(state, reason, with);
    }

    static QStringList menuTexts(const TrayIcon &tray)
    {
        QStringList texts;
        for (QAction *action : tray.item()->contextMenu()->actions()) {
            if (!action->isSeparator() && action->isVisible()) {
                texts << action->text();
            }
        }
        return texts;
    }

private Q_SLOTS:
    void initTestCase()
    {
        // As in main(): a closed window never ends the application by itself.
        QApplication::setQuitOnLastWindowClosed(false);
    }

    void init()
    {
        m_now = Now;
    }

    void cleanup()
    {
        m_app.reset();
        m_accounts.reset();
        m_manager.reset();
        m_status = nullptr;
        m_sync = nullptr;
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
    }

    void upToDateSaysWhenItLastChecked()
    {
        startSynced();
        QCOMPARE(m_status->iconName(), QStringLiteral("state-ok"));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));
        QCOMPARE(m_status->attention(), QString());
    }

    /// The line ages from the clock, every 10 s, with nothing asked of the daemon.
    void theStatusLineAgesOnItsOwn()
    {
        startSynced();
        auto *timer = m_status->findChild<QTimer *>();
        QVERIFY(timer);
        QVERIFY(timer->isActive());
        QCOMPARE(timer->interval(), AccountStatus::TickMs);
        QCOMPARE(AccountStatus::TickMs, 10000);

        // Only the clock moves: the daemon's properties stay as they were.
        m_now += 10;
        QSignalSpy changed(m_status, &AccountStatus::changed);
        m_status->tick();
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 30 s ago"));
        QCOMPARE(changed.count(), 1);
        m_now += 600;
        m_status->tick();
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 10 min ago"));
    }

    void listingIsSyncing()
    {
        startSynced();
        decide("syncing", "listing", {{QStringLiteral("State"), QStringLiteral("listing")}, {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(12)}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-sync"));
        QCOMPARE(m_status->text(), QStringLiteral("Listing your OneDrive: 12 items so far"));
        QVERIFY(m_status->attention().isEmpty());
    }

    void aDownloadIsSyncing()
    {
        startSynced();
        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}});
        decide("syncing", "transferring");
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-sync"));
        QCOMPARE(m_status->text(), QStringLiteral("Downloading 1 file · checked 20 s ago"));
        m_daemon->sync->setTransfers({});
        decide("ok", "up-to-date");
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
    }

    /// Changes waiting or going up are syncing; blocked ones and held
    /// removals need attention; a pause is its own state.
    void uploadsBlockedHeldAndPaused()
    {
        startSynced();
        m_daemon->sync->queue->set({{QStringLiteral("PendingCount"), QVariant::fromValue<uint>(3)}});
        decide("syncing", "transferring");
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        QCOMPARE(m_status->text(), QStringLiteral("3 changes waiting to upload · checked 20 s ago"));
        m_daemon->sync->setUploads({{Root + QStringLiteral("/a.odt"), 1, 10}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("Uploading 1 file · checked 20 s ago"));

        m_daemon->sync->queue->set({{QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(2)}});
        decide("warning", "blocked");
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(m_status->attention(), QStringLiteral("2 changes cannot be uploaded"));

        m_daemon->sync->holdDeletes(Root + QStringLiteral("/old"), 1);
        decide("warning", "deletes-held");
        QTRY_COMPARE(m_status->attention(), QStringLiteral("1 item deleted here waits for you: delete it in OneDrive too, or restore it"));

        m_daemon->sync->queue->RestoreDeletes();
        m_daemon->sync->queue->set({{QStringLiteral("BlockedCount"), QVariant::fromValue<uint>(0)}, {QStringLiteral("PendingCount"), QVariant::fromValue<uint>(0)}});
        m_daemon->sync->setUploads({});
        m_daemon->sync->folder->Pause(0);
        decide("paused", "paused");
        QTRY_COMPARE(m_status->state(), QStringLiteral("paused"));
        QVERIFY(m_status->attention().isEmpty());
        QCOMPARE(m_status->iconName(), QStringLiteral("media-playback-pause"));
        QCOMPARE(m_status->text(), QStringLiteral("Paused · checked 20 s ago"));
        QVERIFY(AppStatus::rank(QStringLiteral("paused")) < AppStatus::rank(QStringLiteral("syncing")));
        QVERIFY(AppStatus::rank(QStringLiteral("offline")) < AppStatus::rank(QStringLiteral("paused")));
    }

    /// While the notification socket is up (LiveChanges "connected") the line says "live" in
    /// place of when it last checked, and stops ageing; otherwise it is as before.
    void liveChangesInTheStatusLine()
    {
        startSynced();
        auto *timer = m_status->findChild<QTimer *>();
        QVERIFY(timer);
        m_daemon->sync->folder->set({{QStringLiteral("LiveChanges"), QStringLiteral("connected")}});
        QTRY_COMPARE(m_sync->liveChanges(), QStringLiteral("connected"));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · live"));
        QCOMPARE(m_status->state(), QStringLiteral("ok"));
        QVERIFY(!timer->isActive());

        m_daemon->sync->queue->set({{QStringLiteral("PendingCount"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("2 changes waiting to upload · live"));

        m_daemon->sync->folder->set({{QStringLiteral("LiveChanges"), QStringLiteral("connecting")}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("2 changes waiting to upload · checked 20 s ago"));
        QVERIFY(timer->isActive());
    }

    /// An account that holds back by itself (HeldBack) is paused, for its line and the tray,
    /// and its line says why; the user's pause, when there is one too, is what the line says.
    void aHeldAccountIsPaused()
    {
        startSynced();
        TrayIcon tray(m_app.get());
        decide("paused", "held-back", {{QStringLiteral("HeldBack"), QStringLiteral("metered")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("paused"));
        QCOMPARE(m_status->text(), QStringLiteral("Paused: metered connection · checked 20 s ago"));
        QTRY_COMPARE(m_app->state(), QStringLiteral("paused"));
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("media-playback-pause"));
        QCOMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("Paused: metered connection · checked 20 s ago"));

        m_daemon->sync->folder->set({{QStringLiteral("HeldBack"), QStringLiteral("on-battery")}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("Paused: on battery · checked 20 s ago"));
        m_daemon->sync->folder->Pause(0);
        QTRY_COMPARE(m_status->text(), QStringLiteral("Paused · checked 20 s ago"));

        // Something that needs the user still comes first, when the daemon says so: the
        // line keeps saying the pause.
        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(1)}});
        decide("warning", "conflicts");
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->text(), QStringLiteral("Paused · checked 20 s ago"));
        QCOMPARE(m_status->attention(), QStringLiteral("1 changed file was moved out of the way"));

        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(0)}});
        m_daemon->sync->folder->Resume();
        decide("ok", "up-to-date", {{QStringLiteral("HeldBack"), QString()}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-ok"));
    }

    /// "Sync Anyway" in the tray: shown while an account holds back by itself and is not paused
    /// by the user; it lifts the hold of every such account, and of no other.
    void theTraySyncsAnywayEveryHeldAccount()
    {
        startSynced();
        FakeAccountObject *family = addFamily();
        FakeAccountObject *work = addFamily();
        QTRY_COMPARE(m_accounts->count(), 3);
        TrayIcon tray(m_app.get());
        QVERIFY(!tray.syncAnywayAction()->isVisible());

        // Work is paused by the user and held too: Resume Syncing is its way back.
        work->sync->folder->set({{QStringLiteral("HeldBack"), QStringLiteral("metered")}});
        work->sync->folder->Pause(0);
        QTRY_VERIFY(tray.resumeAction()->isVisible());
        QVERIFY(!tray.syncAnywayAction()->isVisible());

        m_daemon->sync->folder->set({{QStringLiteral("HeldBack"), QStringLiteral("metered")}});
        family->sync->folder->set({{QStringLiteral("HeldBack"), QStringLiteral("on-battery")}});
        QTRY_VERIFY(tray.syncAnywayAction()->isVisible());
        QVERIFY(menuTexts(tray).contains(QStringLiteral("Sync Anyway")));

        tray.syncAnywayAction()->trigger();
        QTRY_VERIFY(m_daemon->sync->calls.contains(QStringLiteral("SyncAnyway")));
        QTRY_VERIFY(family->sync->calls.contains(QStringLiteral("SyncAnyway")));
        QTRY_VERIFY(!tray.syncAnywayAction()->isVisible());
        QVERIFY(!work->sync->calls.contains(QStringLiteral("SyncAnyway")));
        QCOMPARE(work->sync->folder->heldBack(), QStringLiteral("metered"));
    }

    /// A full OneDrive, and a file too big for the space left, need
    /// attention: one line for the account, not one per file (issue #2).
    void aFullOneDriveNeedsAttention()
    {
        startSynced();
        m_daemon->sync->queue->set({{QStringLiteral("QuotaFull"), true}, {QStringLiteral("QuotaWaitingCount"), QVariant::fromValue<uint>(29)}});
        decide("warning", "quota-full");
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->attention(), QStringLiteral("OneDrive is full: 29 files wait for space"));
        m_daemon->sync->queue->set({{QStringLiteral("QuotaFull"), false}, {QStringLiteral("TooBigCount"), QVariant::fromValue<uint>(1)}});
        decide("warning", "too-big");
        QTRY_COMPARE(m_status->attention(), QStringLiteral("1 file is too big for the space left in OneDrive"));
        m_daemon->sync->queue->set({{QStringLiteral("TooBigCount"), QVariant::fromValue<uint>(0)}});
        decide("ok", "up-to-date");
        QTRY_VERIFY(m_status->attention().isEmpty());
    }

    void aSyncErrorNeedsAttention()
    {
        startSynced();
        decide("warning", "stopped", {{QStringLiteral("State"), QStringLiteral("error")}, {QStringLiteral("Trouble"), QStringLiteral("the folder belongs to another account")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
        // What the folder stopped on, as the daemon says it.
        QCOMPARE(m_status->text(), QStringLiteral("the folder belongs to another account"));
        QVERIFY(m_status->attention().isEmpty());
        decide("warning", "stopped", {{QStringLiteral("Trouble"), QString()}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("Syncing has stopped"));
    }

    /// A conflict needs attention, and that outranks a download under way.
    /// A folder that is not up yet, with nothing known to be wrong, is calm: no warning,
    /// and it turns into one only when the daemon says "error".
    void aFolderNotUpYetIsStartingNotAnError()
    {
        startSynced();
        decide("syncing", "starting", {{QStringLiteral("State"), QStringLiteral("waiting")}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("Starting…"));
        QCOMPARE(m_status->state(), QStringLiteral("syncing"));
        QVERIFY(m_status->attention().isEmpty());
        decide("warning", "stopped", {{QStringLiteral("State"), QStringLiteral("error")}, {QStringLiteral("Trouble"), QStringLiteral("the konedrive helper is not running")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->text(), QStringLiteral("the konedrive helper is not running"));
    }

    void aConflictNeedsAttention()
    {
        startSynced();
        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}});
        decide("syncing", "transferring");
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(1)}});
        decide("warning", "conflicts");
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(m_status->attention(), QStringLiteral("1 changed file was moved out of the way"));
        QCOMPARE(m_status->text(), QStringLiteral("Downloading 1 file · checked 20 s ago"));
        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(0)}});
        decide("syncing", "transferring");
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        QVERIFY(m_status->attention().isEmpty());
    }

    /// A file changed in OneDrive that could not be updated here: the daemon
    /// says so in NotUpdated while the folder stays ready.
    void aFailedUpdateNeedsAttention()
    {
        startSynced();
        const QString note = QStringLiteral("1 file(s) changed in OneDrive could not be updated here yet: the connection was reset");
        decide("warning", "not-updated", {{QStringLiteral("NotUpdated"), note}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->attention(), note);
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));
        // The note is the attention only for its own reason.
        decide("warning", "conflicts");
        QTRY_VERIFY(m_status->attention() != note);
        decide("ok", "up-to-date", {{QStringLiteral("NotUpdated"), QString()}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        QVERIFY(m_status->attention().isEmpty());
    }

    /// The helper's trouble, when the daemon counts it against this account: a warning
    /// that says so, while the line says what the folder does.
    void theHelpersTroubleNeedsAttention()
    {
        startSynced();
        decide("warning", "helper-unavailable");
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QVERIFY2(m_status->attention().contains(QStringLiteral("helper")), qPrintable(m_status->attention()));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));

        // The helper's state alone decides nothing here: the daemon says whose concern it is.
        decide("ok", "up-to-date");
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("stopped")}});
        QTRY_VERIFY(m_manager->helperTrouble());
        QCOMPARE(m_status->state(), QStringLiteral("ok"));
        QVERIFY(m_status->attention().isEmpty());
    }

    /// Trouble that does not stop the folder (no network, say) is said in Trouble: the
    /// line shows it, capitalised, and the tray looks offline when the daemon says the
    /// account cannot reach OneDrive (review B7).
    void troubleThatDoesNotStopTheFolderShowsAndLooksOffline()
    {
        startSynced();
        m_now = Now + 7200;
        const QString offline = QStringLiteral("cannot reach OneDrive (error sending request); trying again");
        decide("offline", "unreachable", {{QStringLiteral("Trouble"), offline}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-offline"));
        const QString line = QStringLiteral("Cannot reach OneDrive (error sending request); trying again · checked 2 h ago");
        QCOMPARE(m_status->text(), line);
        QVERIFY(m_status->attention().isEmpty());

        // A failed update as well: that needs attention; the line still says why.
        const QString note = QStringLiteral("1 file(s) changed in OneDrive could not be updated here yet: timed out");
        decide("warning", "not-updated", {{QStringLiteral("NotUpdated"), note}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->attention(), note);
        QCOMPARE(m_status->text(), line);

        // And under a pause, which the daemon ranks over it: the line says the trouble.
        m_daemon->sync->folder->Pause(0);
        decide("paused", "paused", {{QStringLiteral("NotUpdated"), QString()}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("paused"));
        QCOMPARE(m_status->text(), line);

        m_daemon->sync->folder->Resume();
        decide("ok", "up-to-date", {{QStringLiteral("Trouble"), QString()}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 2 h ago"));
    }

    /// M4: only what the daemon calls unreachable looks offline; any other trouble
    /// that does not stop the folder is a warning, by the reason and not by the sentence.
    void otherTroubleLooksLikeAWarningNotOffline()
    {
        startSynced();
        decide("warning", "trouble", {{QStringLiteral("Trouble"), QStringLiteral("cannot reach OneDrive, and part of the folder is scanned")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(m_status->text(), QStringLiteral("Cannot reach OneDrive, and part of the folder is scanned · checked 20 s ago"));
        QVERIFY(m_status->attention().isEmpty());
    }

    /// Nothing is read from LastError: whatever it says, the state, the line and the
    /// attention are what the daemon decided (I2).
    void nothingIsReadFromLastError()
    {
        startSynced();
        m_daemon->sync->folder->set(
            {{QStringLiteral("LastError"),
              QStringLiteral("cannot reach OneDrive (error sending request); trying again. 1 file(s) changed in OneDrive could not be updated here yet: timed out")}});
        // Something the window does follow, sent after it.
        m_daemon->sync->folder->set({{QStringLiteral("LiveChanges"), QStringLiteral("connected")}});
        QTRY_COMPARE(m_status->text(), QStringLiteral("Up to date · live"));
        QCOMPARE(m_status->state(), QStringLiteral("ok"));
        QVERIFY(m_status->attention().isEmpty());
    }

    /// A state this build does not know, or none at all (a daemon of an older build has
    /// no Overall), reads offline; the line is still put together.
    void aStateTheWindowDoesNotKnowReadsOffline()
    {
        startSynced();
        decide("on-fire", "burning");
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-offline"));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));
        decide("ok", "up-to-date");
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        decide("", "");
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
    }

    /// With no system tray to come back from, closing the window quits
    /// rather than leave an invisible process (review B2).
    void closingTheWindowWithoutATrayQuits()
    {
        startSynced();
        TrayIcon tray(m_app.get());
        QWindow window;
        tray.setWindow(&window);
        tray.showWindow();
        QTRY_VERIFY(window.isVisible());
        QVERIFY(!tray.trayAvailable());
        QSignalSpy quit(&tray, &TrayIcon::quitRequested);
        window.close();
        QCOMPARE(quit.count(), 1);
    }

    /// With a tray, closing the window only hides it; the tray host is
    /// followed as it comes and goes.
    void closingTheWindowWithATrayKeepsRunning()
    {
        startSynced();
        FakeTrayWatcher watcher;
        QVERIFY(watcher.start(fake::bus()));
        TrayIcon tray(m_app.get());
        QTRY_VERIFY(tray.trayAvailable());
        QWindow window;
        tray.setWindow(&window);
        tray.showWindow();
        QTRY_VERIFY(window.isVisible());
        QSignalSpy quit(&tray, &TrayIcon::quitRequested);
        window.close();
        QVERIFY(!window.isVisible());
        QCOMPARE(quit.count(), 0);

        watcher.stop();
        QTRY_VERIFY(!tray.trayAvailable());
    }

    void signedOutIsOffline()
    {
        startSynced();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        decide("offline", "signed-out");
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-offline"));
        QCOMPARE(m_status->text(), QStringLiteral("Signed out of OneDrive"));
        decide("offline", "signing-in");
        QTRY_COMPARE(m_status->text(), QStringLiteral("Signing in…"));
        QCOMPARE(m_status->state(), QStringLiteral("offline"));
    }

    void noFolderIsOffline()
    {
        startSynced();
        decide("offline", "no-folder", {{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}, {QStringLiteral("Source"), QString()}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->text(), QStringLiteral("No OneDrive folder yet"));
    }

    void aStoppedDaemonIsOffline()
    {
        startSynced();
        m_daemon->stop();
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-offline"));
        QCOMPARE(m_status->text(), QStringLiteral("The KOneDrive service is not running"));
    }

    void theTrayShowsTheStateAndTheStatusLine()
    {
        startSynced();
        TrayIcon tray(m_app.get());
        QCOMPARE(tray.item()->iconName(), QStringLiteral("state-ok"));
        QCOMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("Up to date · checked 20 s ago"));

        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(2)}});
        decide("warning", "conflicts");
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("Up to date · checked 20 s ago\n2 changed files were moved out of the way"));

        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        decide("offline", "signed-out");
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-offline"));
        QCOMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("Signed out of OneDrive"));
    }

    /// A click on the icon shows the hidden window, and hides it again.
    void activatingTheTrayTogglesTheWindow()
    {
        startSynced();
        TrayIcon tray(m_app.get());
        QWindow window;
        tray.setWindow(&window);
        QVERIFY(!window.isVisible());
        tray.item()->activate();
        QTRY_VERIFY(window.isVisible());
        QTRY_VERIFY(window.isActive());
        tray.item()->activate();
        QTRY_VERIFY(!window.isVisible());
    }

    void theTrayMenu()
    {
        startSynced();
        TrayIcon tray(m_app.get());
        QWindow window;
        tray.setWindow(&window);

        QCOMPARE(menuTexts(tray), (QStringList{QStringLiteral("Open OneDrive Folder"), QStringLiteral("Open KOneDrive"), QStringLiteral("Refresh Now"), QStringLiteral("Pause Syncing"), QStringLiteral("Quit")}));

        QVERIFY(tray.openFolderAction()->isEnabled());
        tray.refreshAction()->trigger();
        QTRY_VERIFY(m_daemon->sync->calls.contains(QStringLiteral("Refresh")));

        tray.openWindowAction()->trigger();
        QTRY_VERIFY(window.isVisible());

        QSignalSpy quit(&tray, &TrayIcon::quitRequested);
        tray.quitAction()->trigger();
        QCOMPARE(quit.count(), 1);

        // Without a folder there is nothing to open or refresh.
        m_daemon->sync->folder->set({{QStringLiteral("Path"), QString()}, {QStringLiteral("State"), QStringLiteral("none")}, {QStringLiteral("Source"), QString()}});
        QTRY_VERIFY(!tray.openFolderAction()->isEnabled());
        QVERIFY(!tray.refreshAction()->isEnabled());
    }

    /// With no account the tray is offline and has nothing to open; its
    /// tooltip says whether the service is there at all.
    void noAccountIsOffline()
    {
        m_daemon = std::make_unique<FakeDaemon>(QStringList{});
        QVERIFY(m_daemon->start());
        follow(0);
        QTRY_VERIFY(m_manager->serviceAvailable());
        TrayIcon tray(m_app.get());
        QCOMPARE(m_app->state(), QStringLiteral("offline"));
        QCOMPARE(tray.item()->iconName(), QStringLiteral("state-offline"));
        QTRY_COMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("No OneDrive account yet"));
        QVERIFY(!tray.openFolderAction()->isEnabled());
        QVERIFY(!tray.refreshAction()->isEnabled());

        m_daemon->stop();
        QTRY_COMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("The KOneDrive service is not running"));
    }

    /// Several accounts: the icon is the worst state — needs attention, then
    /// signed out, then syncing, then synced — and the tooltip has a line
    /// per account, in order, what needs attention in place of the status.
    void theTrayShowsTheWorstStateAndALinePerAccount()
    {
        startSynced();
        FakeAccountObject *family = addFamily();
        QTRY_COMPARE(m_accounts->count(), 2);
        TrayIcon tray(m_app.get());
        QTRY_COMPARE(tray.item()->toolTipSubTitle(),
                     QStringLiteral("Personal — Up to date · checked 20 s ago\nFamily — Up to date · checked 1 min ago"));
        QCOMPARE(tray.item()->iconName(), QStringLiteral("state-ok"));

        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}});
        decide("syncing", "transferring");
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-sync"));

        // A paused account is worse than one syncing, and one offline worse than both.
        family->sync->folder->decide("paused", "held-back", {{QStringLiteral("HeldBack"), QStringLiteral("metered")}});
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("media-playback-pause"));

        family->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        family->sync->folder->decide("offline", "signed-out", {{QStringLiteral("HeldBack"), QString()}});
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-offline"));
        QTRY_COMPARE(tray.item()->toolTipSubTitle(),
                     QStringLiteral("Personal — Downloading 1 file · checked 20 s ago\nFamily — Signed out of OneDrive"));

        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(2)}});
        decide("warning", "conflicts");
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-warning"));
        QTRY_COMPARE(tray.item()->toolTipSubTitle(),
                     QStringLiteral("Personal — 2 changed files were moved out of the way\nFamily — Signed out of OneDrive"));
        QCOMPARE(AppStatus::rank(QStringLiteral("warning")) < AppStatus::rank(QStringLiteral("offline")), true);

        // Renaming an account renames its line.
        family->account->set({{QStringLiteral("Label"), QStringLiteral("Home")}});
        QTRY_VERIFY(tray.item()->toolTipSubTitle().endsWith(QStringLiteral("\nHome — Signed out of OneDrive")));
    }

    /// "Pause Syncing" pauses every OneDrive folder; the tray shows the pause,
    /// and "Resume Syncing" ends it for each.
    void theTrayPausesAndResumesEveryAccount()
    {
        startSynced();
        FakeAccountObject *family = addFamily();
        QTRY_COMPARE(m_accounts->count(), 2);
        TrayIcon tray(m_app.get());
        // The second account's properties load after its row is there, and the
        // tray passes over an account with no folder loaded yet: its line says
        // when it has one.
        QTRY_COMPARE(tray.item()->toolTipSubTitle(),
                     QStringLiteral("Personal — Up to date · checked 20 s ago\nFamily — Up to date · checked 1 min ago"));
        QVERIFY(!tray.resumeAction()->isVisible());
        QCOMPARE(tray.pauseMenu()->actions().size(), 4);

        tray.pauseMenu()->actions().at(0)->trigger(); // for 2 hours
        QTRY_VERIFY(m_daemon->sync->calls.contains(QStringLiteral("Pause:7200")));
        QTRY_VERIFY(family->sync->calls.contains(QStringLiteral("Pause:7200")));
        decide("paused", "paused");
        family->sync->folder->decide("paused", "paused");
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("media-playback-pause"));
        QTRY_VERIFY(tray.resumeAction()->isVisible());
        QVERIFY(!tray.pauseMenuAction()->isVisible());

        tray.resumeAction()->trigger();
        QTRY_VERIFY(m_daemon->sync->calls.contains(QStringLiteral("Resume")));
        QTRY_VERIFY(family->sync->calls.contains(QStringLiteral("Resume")));
        decide("ok", "up-to-date");
        family->sync->folder->decide("ok", "up-to-date");
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-ok"));
    }

    /// Several accounts: "Open Folder" lists the folders; Refresh Now asks every OneDrive folder.
    void theTrayMenuWithSeveralAccounts()
    {
        startSynced();
        FakeAccountObject *family = addFamily();
        FakeAccountObject *work = m_daemon->addAccount(QStringLiteral("Work"));
        QTRY_COMPARE(m_accounts->count(), 3);
        TrayIcon tray(m_app.get());
        QTRY_COMPARE(menuTexts(tray), (QStringList{QStringLiteral("Open Folder"), QStringLiteral("Open KOneDrive"), QStringLiteral("Refresh Now"), QStringLiteral("Pause Syncing"), QStringLiteral("Quit")}));
        QVERIFY(!tray.openFolderAction()->isVisible());

        // Work has no folder: no entry.
        QTRY_COMPARE(tray.openFolderMenu()->actions().size(), 2);
        QCOMPARE(tray.openFolderMenu()->actions().at(0)->text(), QStringLiteral("Personal"));
        QCOMPARE(tray.openFolderMenu()->actions().at(1)->text(), QStringLiteral("Family"));
        QCOMPARE(tray.openFolderMenu()->actions().at(1)->toolTip(), QStringLiteral("/home/u/Family"));
        QVERIFY(tray.openFolderMenuAction()->isEnabled());
        // An "&" in a label is shown, not taken for a mnemonic.
        family->account->set({{QStringLiteral("Label"), QStringLiteral("Kids & Me")}});
        QTRY_COMPARE(tray.openFolderMenu()->actions().at(1)->text(), QStringLiteral("Kids && Me"));

        tray.refreshAction()->trigger();
        QTRY_VERIFY(m_daemon->sync->calls.contains(QStringLiteral("Refresh")));
        QTRY_VERIFY(family->sync->calls.contains(QStringLiteral("Refresh")));
        QVERIFY(!work->sync->calls.contains(QStringLiteral("Refresh")));

        // Back to one account: "Open OneDrive Folder" again.
        m_daemon->removeAccount(family->path);
        m_daemon->removeAccount(work->path);
        QTRY_COMPARE(menuTexts(tray), (QStringList{QStringLiteral("Open OneDrive Folder"), QStringLiteral("Open KOneDrive"), QStringLiteral("Refresh Now"), QStringLiteral("Pause Syncing"), QStringLiteral("Quit")}));
        QVERIFY(tray.openFolderAction()->isEnabled());
    }

    /// A click opens the window on the one account needing attention, when
    /// exactly one does; otherwise on the account it was showing.
    void aClickShowsTheOneAccountNeedingAttention()
    {
        startSynced();
        FakeAccountObject *family = addFamily();
        QTRY_COMPARE(m_accounts->count(), 2);
        TrayIcon tray(m_app.get());
        QWindow window;
        tray.setWindow(&window);
        QSignalSpy toShow(&tray, &TrayIcon::accountToShow);

        family->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(1)}});
        family->sync->folder->decide("warning", "conflicts");
        QTRY_COMPARE(m_accounts->at(1)->status()->state(), QStringLiteral("warning"));
        tray.item()->activate();
        QTRY_VERIFY(window.isVisible());
        QCOMPARE(toShow.count(), 1);
        QCOMPARE(toShow.at(0).at(0).toString(), family->path);

        window.hide();
        m_daemon->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(1)}});
        decide("warning", "conflicts");
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        tray.item()->activate();
        QTRY_VERIFY(window.isVisible());
        QCOMPARE(toShow.count(), 1);
    }
};

QTEST_MAIN(AppStatusTest)

#include "appstatustest.moc"
