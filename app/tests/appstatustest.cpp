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
/// the private bus and a fake clock.
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
        family->sync->set({{QStringLiteral("RootPath"), QStringLiteral("/home/u/Family")},
                           {QStringLiteral("RootState"), QStringLiteral("ready")},
                           {QStringLiteral("RootSource"), QStringLiteral("onedrive")},
                           {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(Now - 90)}});
        return family;
    }

    /// Signed in, a OneDrive folder that is ready, checked 20 s ago: "ok".
    void startSynced()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        m_daemon->sync->set({{QStringLiteral("RootPath"), Root},
                             {QStringLiteral("RootState"), QStringLiteral("ready")},
                             {QStringLiteral("RootSource"), QStringLiteral("onedrive")},
                             {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(Now - 20)}});
        QVERIFY(m_daemon->start());
        follow();
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
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
        m_daemon->sync->set({{QStringLiteral("RootState"), QStringLiteral("listing")}, {QStringLiteral("ItemsListed"), QVariant::fromValue<qulonglong>(12)}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-sync"));
        QCOMPARE(m_status->text(), QStringLiteral("Listing your OneDrive: 12 items so far"));
    }

    void aDownloadIsSyncing()
    {
        startSynced();
        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-sync"));
        QCOMPARE(m_status->text(), QStringLiteral("Downloading 1 file · checked 20 s ago"));
        m_daemon->sync->setTransfers({});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
    }

    void aSyncErrorNeedsAttention()
    {
        startSynced();
        m_daemon->sync->set({{QStringLiteral("RootState"), QStringLiteral("error")}, {QStringLiteral("LastError"), QStringLiteral("The folder belongs to another account")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(m_status->text(), QStringLiteral("The folder belongs to another account"));
    }

    /// A conflict needs attention, and that outranks a download under way.
    void aConflictNeedsAttention()
    {
        startSynced();
        m_daemon->sync->setTransfers({{Root + QStringLiteral("/a.iso"), 1, 10}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
        m_daemon->sync->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(1)}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(m_status->attention(), QStringLiteral("1 changed file was moved out of the way"));
        m_daemon->sync->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(0)}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("syncing"));
    }

    /// A file changed in OneDrive that could not be updated here: the daemon
    /// says so in LastError while the folder stays ready.
    void aFailedUpdateNeedsAttention()
    {
        startSynced();
        const QString note = QStringLiteral("1 file(s) changed in OneDrive could not be updated here yet: the connection was reset");
        m_daemon->sync->set({{QStringLiteral("LastError"), note}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->attention(), note);
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));
        m_daemon->sync->set({{QStringLiteral("LastError"), QString()}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
    }

    /// The helper serves every account (Accounts1.HelperState): its trouble
    /// is this account's warning too, while the folder itself stays ready.
    void theHelpersTroubleNeedsAttention()
    {
        startSynced();
        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("stopped")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QVERIFY2(m_status->attention().contains(QStringLiteral("helper")), qPrintable(m_status->attention()));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));

        // A folder the helper does not intercept is not its concern (M7).
        m_daemon->sync->set({{QStringLiteral("RootState"), QStringLiteral("no-interception")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        m_daemon->sync->set({{QStringLiteral("RootState"), QStringLiteral("ready")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));

        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("connected")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
    }

    /// Trouble that does not stop the folder (no network, say) keeps RootState
    /// ready and is said in LastError: the line shows it, the tray looks
    /// offline (review B7).
    void troubleThatDoesNotStopTheFolderShowsAndLooksOffline()
    {
        startSynced();
        m_now = Now + 7200;
        const QString offline = QStringLiteral("cannot reach OneDrive (error sending request); trying again");
        m_daemon->sync->set({{QStringLiteral("LastError"), offline}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-offline"));
        const QString line = QStringLiteral("Cannot reach OneDrive (error sending request); trying again · checked 2 h ago");
        QCOMPARE(m_status->text(), line);

        // A failed update as well: that needs attention; the line still says why.
        const QString note = QStringLiteral("1 file(s) changed in OneDrive could not be updated here yet: timed out");
        m_daemon->sync->set({{QStringLiteral("LastError"), QString(offline + QStringLiteral(". ") + note)}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->attention(), note);
        QCOMPARE(m_status->text(), line);

        // The rescue note is about conflicts, which have their own state: it is not trouble.
        m_daemon->sync->set({{QStringLiteral("LastError"), QStringLiteral("1 file(s) changed here were moved to /r because the cloud changed or removed them")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 2 h ago"));
    }

    /// M4: only "cannot reach OneDrive" trouble looks offline; any other
    /// trouble that does not stop the folder looks like a warning instead.
    void otherTroubleLooksLikeAWarningNotOffline()
    {
        startSynced();
        m_daemon->sync->set({{QStringLiteral("LastError"), QStringLiteral("something else went wrong; trying again")}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-warning"));
    }

    /// I2: no-interception must not hide LastError's trouble behind the
    /// fixed warning ("this folder is registered WITHOUT interception…"),
    /// which itself carries no ". " and needs no attention on its own.
    void noInterceptionShowsItsOwnTrouble()
    {
        const QString warning = QStringLiteral(
            "this folder is registered WITHOUT interception: nothing fills a placeholder when it "
            "is opened, so files in this folder read as zeros until they are explicitly hydrated");
        m_daemon = std::make_unique<FakeDaemon>();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        m_daemon->sync->set({{QStringLiteral("RootPath"), Root},
                             {QStringLiteral("RootState"), QStringLiteral("no-interception")},
                             {QStringLiteral("RootSource"), QStringLiteral("onedrive")},
                             {QStringLiteral("LastChecked"), QVariant::fromValue<qlonglong>(Now - 20)},
                             {QStringLiteral("LastError"), QString(warning + QStringLiteral(". cannot reach OneDrive (error sending request); trying again"))}});
        QVERIFY(m_daemon->start());
        follow();
        // "offline" alone is ambiguous (it is also what "no service yet" looks
        // like): wait for the service first, so the text check below is not
        // satisfied by that same coincidence.
        QTRY_VERIFY(m_sync->serviceAvailable());
        QTRY_COMPARE(m_status->text(), QStringLiteral("Cannot reach OneDrive (error sending request); trying again · checked 20 s ago"));
        QCOMPARE(m_status->state(), QStringLiteral("offline"));

        // Just the warning, nothing else wrong: no-interception alone needs no attention.
        m_daemon->sync->set({{QStringLiteral("LastError"), warning}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("ok"));
        QCOMPARE(m_status->text(), QStringLiteral("Up to date · checked 20 s ago"));
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
        QTRY_COMPARE(m_status->state(), QStringLiteral("offline"));
        QCOMPARE(m_status->iconName(), QStringLiteral("state-offline"));
        QCOMPARE(m_status->text(), QStringLiteral("Signed out of OneDrive"));
    }

    void noFolderIsOffline()
    {
        startSynced();
        m_daemon->sync->set({{QStringLiteral("RootPath"), QString()}, {QStringLiteral("RootState"), QStringLiteral("none")}, {QStringLiteral("RootSource"), QString()}});
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

        m_daemon->sync->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-warning"));
        QCOMPARE(tray.item()->toolTipSubTitle(), QStringLiteral("Up to date · checked 20 s ago\n2 changed files were moved out of the way"));

        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
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

        QCOMPARE(menuTexts(tray), (QStringList{QStringLiteral("Open OneDrive Folder"), QStringLiteral("Open KOneDrive"), QStringLiteral("Refresh Now"), QStringLiteral("Quit")}));

        QVERIFY(tray.openFolderAction()->isEnabled());
        tray.refreshAction()->trigger();
        QTRY_VERIFY(m_daemon->sync->calls.contains(QStringLiteral("Refresh")));

        tray.openWindowAction()->trigger();
        QTRY_VERIFY(window.isVisible());

        QSignalSpy quit(&tray, &TrayIcon::quitRequested);
        tray.quitAction()->trigger();
        QCOMPARE(quit.count(), 1);

        // Without a folder there is nothing to open or refresh.
        m_daemon->sync->set({{QStringLiteral("RootPath"), QString()}, {QStringLiteral("RootState"), QStringLiteral("none")}, {QStringLiteral("RootSource"), QString()}});
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
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-sync"));

        family->account->set({{QStringLiteral("State"), QStringLiteral("signed-out")}});
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-offline"));
        QTRY_COMPARE(tray.item()->toolTipSubTitle(),
                     QStringLiteral("Personal — Downloading 1 file · checked 20 s ago\nFamily — Signed out of OneDrive"));

        m_daemon->sync->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(2)}});
        QTRY_COMPARE(tray.item()->iconName(), QStringLiteral("state-warning"));
        QTRY_COMPARE(tray.item()->toolTipSubTitle(),
                     QStringLiteral("Personal — 2 changed files were moved out of the way\nFamily — Signed out of OneDrive"));
        QCOMPARE(AppStatus::rank(QStringLiteral("warning")) < AppStatus::rank(QStringLiteral("offline")), true);

        // Renaming an account renames its line.
        family->account->set({{QStringLiteral("Label"), QStringLiteral("Home")}});
        QTRY_VERIFY(tray.item()->toolTipSubTitle().endsWith(QStringLiteral("\nHome — Signed out of OneDrive")));
    }

    /// Several accounts: "Open Folder" lists the folders; Refresh Now asks every OneDrive folder.
    void theTrayMenuWithSeveralAccounts()
    {
        startSynced();
        FakeAccountObject *family = addFamily();
        FakeAccountObject *work = m_daemon->addAccount(QStringLiteral("Work"));
        QTRY_COMPARE(m_accounts->count(), 3);
        TrayIcon tray(m_app.get());
        QTRY_COMPARE(menuTexts(tray), (QStringList{QStringLiteral("Open Folder"), QStringLiteral("Open KOneDrive"), QStringLiteral("Refresh Now"), QStringLiteral("Quit")}));
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
        QTRY_COMPARE(menuTexts(tray), (QStringList{QStringLiteral("Open OneDrive Folder"), QStringLiteral("Open KOneDrive"), QStringLiteral("Refresh Now"), QStringLiteral("Quit")}));
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

        family->sync->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(1)}});
        QTRY_COMPARE(m_accounts->at(1)->status()->state(), QStringLiteral("warning"));
        tray.item()->activate();
        QTRY_VERIFY(window.isVisible());
        QCOMPARE(toShow.count(), 1);
        QCOMPARE(toShow.at(0).at(0).toString(), family->path);

        window.hide();
        m_daemon->sync->set({{QStringLiteral("ConflictCount"), QVariant::fromValue<uint>(1)}});
        QTRY_COMPARE(m_status->state(), QStringLiteral("warning"));
        tray.item()->activate();
        QTRY_VERIFY(window.isVisible());
        QCOMPARE(toShow.count(), 1);
    }
};

QTEST_MAIN(AppStatusTest)

#include "appstatustest.moc"
