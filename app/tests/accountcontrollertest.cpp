#include "accountcontroller.h"
#include "fakedaemon.h"

#include <QSignalSpy>
#include <QTest>

#include <memory>

/// One account's Account1, at its own path, from the fake daemon on the private bus.
class AccountControllerTest : public QObject
{
    Q_OBJECT

private:
    std::unique_ptr<FakeDaemon> m_daemon;

    void startFake()
    {
        m_daemon = std::make_unique<FakeDaemon>();
        QVERIFY(m_daemon->start());
    }

private Q_SLOTS:
    void cleanup()
    {
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
    }

    void reportsUnavailableWithoutDaemon()
    {
        AccountController controller(fake::FirstAccount);
        QTest::qWait(300);
        QVERIFY(!controller.serviceAvailable());
    }

    void readsInitialProperties()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QCOMPARE(controller.id(), fake::idFor(1));
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.state(), QStringLiteral("signed-out"));
        QCOMPARE(controller.label(), QStringLiteral("Personal"));
        QCOMPARE(controller.mode(), QStringLiteral("read-only"));
    }

    void noticesDaemonStartingLater()
    {
        AccountController controller(fake::FirstAccount);
        QTest::qWait(100);
        QVERIFY(!controller.serviceAvailable());
        startFake();
        QTRY_VERIFY(controller.serviceAvailable());
    }

    void followsPropertiesChanged()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        m_daemon->account->set({
            {QStringLiteral("State"), QStringLiteral("signed-in")},
            {QStringLiteral("DisplayName"), QStringLiteral("Test User")},
            {QStringLiteral("QuotaTotal"), QVariant::fromValue<qulonglong>(5368709120ULL)},
        });
        QTRY_COMPARE(controller.state(), QStringLiteral("signed-in"));
        QCOMPARE(controller.displayName(), QStringLiteral("Test User"));
        QCOMPARE(controller.quotaTotal(), 5368709120ULL);
    }

    /// Another account's changes are not this one's.
    void followsOnlyItsOwnPath()
    {
        m_daemon = std::make_unique<FakeDaemon>(QStringList{QStringLiteral("Personal"), QStringLiteral("Family")});
        QVERIFY(m_daemon->start());
        AccountController personal(fake::FirstAccount);
        AccountController family(m_daemon->object(1)->path);
        QTRY_VERIFY(personal.serviceAvailable() && family.serviceAvailable());
        QCOMPARE(family.label(), QStringLiteral("Family"));

        m_daemon->object(1)->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        QTRY_COMPARE(family.state(), QStringLiteral("signed-in"));
        QCOMPARE(personal.state(), QStringLiteral("signed-out"));
    }

    void signInRequestsBrowserAndCancelClearsUrl()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QSignalSpy openSpy(&controller, &AccountController::openUrlRequested);
        controller.signIn();
        QTRY_COMPARE(openSpy.count(), 1);
        const QString url = QStringLiteral("https://login.example/authorize?account=") + fake::idFor(1);
        QCOMPARE(openSpy.at(0).at(0).toString(), url);
        QCOMPARE(controller.signInUrl(), url);
        QTRY_COMPARE(controller.state(), QStringLiteral("signing-in"));

        controller.cancelSignIn();
        QTRY_COMPARE(controller.state(), QStringLiteral("signed-out"));
        QCOMPARE(controller.signInUrl(), QString());
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("CancelSignIn")));
    }

    void renamingShowsRefusalsAndClearsThemOnSuccess()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.setLabel(QStringLiteral("a/b"));
        QTRY_VERIFY(controller.actionError().contains(QStringLiteral("not a label")));

        controller.setLabel(QStringLiteral("  Home "));
        QTRY_COMPARE(controller.label(), QStringLiteral("Home"));
        QVERIFY(controller.actionError().isEmpty());
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("SetLabel:Home")));
    }

    /// A switch to read-write opens its sign-in and waits for it, as the
    /// command line does: until Mode turns read-write, LastError says why
    /// not, or the window gives it up.
    void aSwitchToReadWriteWaitsForItsSignIn()
    {
        startFake();
        m_daemon->account->gateOpen = true;
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("LastError"), QStringLiteral("an older trouble")}});
        AccountController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.lastError(), QStringLiteral("an older trouble"));
        QSignalSpy openSpy(&controller, &AccountController::openUrlRequested);

        // Granted.
        controller.setMode(QStringLiteral("read-write"));
        QCOMPARE(controller.switchingTo(), QStringLiteral("read-write"));
        QTRY_COMPARE(openSpy.count(), 1);
        const QString url = openSpy.at(0).at(0).toString();
        QVERIFY(url.contains(QStringLiteral("Files.ReadWrite")));
        QCOMPARE(controller.signInUrl(), url);
        QVERIFY(controller.modeSignInPending());
        QTRY_COMPARE(controller.lastError(), QString()); // SetMode cleared it
        QTest::qWait(100);
        QVERIFY(controller.modeSignInPending());
        m_daemon->account->grantReadWrite();
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-write"));
        QVERIFY(!controller.modeSignInPending());
        QCOMPARE(controller.switchingTo(), QString());
        QCOMPARE(controller.signInUrl(), QString());
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("SetMode:read-write:no-force")));

        // Refused in the browser: LastError says why, and the mode stays.
        m_daemon->account->set({{QStringLiteral("Mode"), QStringLiteral("read-only")}});
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-only"));
        controller.setMode(QStringLiteral("read-write"));
        QTRY_VERIFY(controller.modeSignInPending());
        m_daemon->account->set({{QStringLiteral("LastError"), QStringLiteral("the sign-in was refused")}});
        QTRY_VERIFY(!controller.modeSignInPending());
        QCOMPARE(controller.switchingTo(), QString());
        QCOMPARE(controller.mode(), QStringLiteral("read-only"));

        // Given up here: the daemon's sign-in is cancelled, the account stays signed in.
        controller.setMode(QStringLiteral("read-write"));
        QTRY_VERIFY(controller.modeSignInPending());
        controller.cancelModeSwitch();
        QVERIFY(!controller.modeSignInPending());
        QTRY_VERIFY(m_daemon->account->calls.contains(QStringLiteral("CancelSignIn")));
        QCOMPARE(controller.state(), QStringLiteral("signed-in"));
        QCOMPARE(openSpy.count(), 3);
    }

    /// Refusals are told in the window's words, never the daemon's; the
    /// development gate does not blame the user.
    void aRefusedSwitchSaysWhyInPlainWords()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QSignalSpy openSpy(&controller, &AccountController::openUrlRequested);

        controller.setMode(QStringLiteral("read-write"));
        QTRY_VERIFY(!controller.actionError().isEmpty());
        QVERIFY2(controller.actionError().startsWith(QStringLiteral("Uploading is not available for this account in this version.")), qPrintable(controller.actionError()));
        QVERIFY(!controller.actionError().contains(QStringLiteral("DAEMON-GATE-WORDS")));
        QCOMPARE(controller.switchingTo(), QString());

        m_daemon->account->gateOpen = true;
        controller.setMode(QStringLiteral("read-write"));
        QTRY_VERIFY2(controller.actionError().startsWith(QStringLiteral("This account is not signed in.")), qPrintable(controller.actionError()));
        QCOMPARE(openSpy.count(), 0);

        QVERIFY(AccountController::modeRefusalText(QStringLiteral("read-write"), QStringLiteral("org.konedrive.Error.ModeNotGranted"), QStringLiteral("x"))
                    .contains(QStringLiteral("Sign in again")));
        QCOMPARE(AccountController::modeRefusalText(QStringLiteral("read-only"), QStringLiteral("org.konedrive.Error.Failed"), QStringLiteral("disk full")),
                 QStringLiteral("Uploading was not turned off: disk full"));
    }

    /// Changes waiting to be uploaded make the switch to read-only a
    /// question; forced, it goes through.
    void aSwitchToReadOnlyAsksBeforeDroppingUploads()
    {
        startFake();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        m_daemon->account->pendingUploads = 3;
        AccountController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-write"));
        QSignalSpy asked(&controller, &AccountController::pendingUploadsRefused);

        controller.setMode(QStringLiteral("read-only"));
        QCOMPARE(controller.switchingTo(), QStringLiteral("read-only"));
        QTRY_COMPARE(asked.count(), 1);
        QCOMPARE(controller.actionError(), QString());
        QCOMPARE(controller.switchingTo(), QString());
        QCOMPARE(controller.mode(), QStringLiteral("read-write"));

        controller.setMode(QStringLiteral("read-only"), true);
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-only"));
        QTRY_COMPARE(controller.switchingTo(), QString());
        QCOMPARE(asked.count(), 1);
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("SetMode:read-only:force")));
    }

    void signOutAndRefreshCallTheDaemon()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.refreshAccountInfo();
        controller.signOut();
        QTRY_VERIFY(m_daemon->account->calls.contains(QStringLiteral("SignOut")));
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("RefreshAccountInfo")));
    }
};

QTEST_GUILESS_MAIN(AccountControllerTest)

#include "accountcontrollertest.moc"
