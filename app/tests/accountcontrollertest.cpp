#include "accountcontroller.h"
#include "fakedaemon.h"

#include <QSignalSpy>
#include <QTest>

#include <memory>

/// One account's Account, at its own path, from the fake daemon on the private bus.
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

    /// Refusals the window knows are told in its own words, never the
    /// daemon's; no account is refused for what it is.
    void aRefusedSwitchSaysWhyInPlainWords()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        QSignalSpy openSpy(&controller, &AccountController::openUrlRequested);

        controller.setMode(QStringLiteral("read-write"));
        QTRY_VERIFY2(controller.actionError().startsWith(QStringLiteral("This account is not signed in.")), qPrintable(controller.actionError()));
        QVERIFY(!controller.actionError().contains(QStringLiteral("sign in first; then switch")));
        QCOMPARE(controller.switchingTo(), QString());
        QCOMPARE(openSpy.count(), 0);

        QVERIFY(AccountController::modeRefusalText(QStringLiteral("read-write"), QStringLiteral("org.konedrive.Error.ModeNotGranted"), QStringLiteral("x"))
                    .contains(QStringLiteral("Sign in again")));
        QCOMPARE(AccountController::modeRefusalText(QStringLiteral("read-only"), QStringLiteral("org.konedrive.Error.Failed"), QStringLiteral("disk full")),
                 QStringLiteral("Read-only was not turned on: disk full"));
    }

    /// A switch to read-only is asked of the daemon as it was asked for,
    /// forced or not, and is no error.
    void aSwitchToReadOnlyIsAskedForcedOrNot()
    {
        startFake();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        AccountController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-write"));

        controller.setMode(QStringLiteral("read-only"));
        QCOMPARE(controller.switchingTo(), QStringLiteral("read-only"));
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-only"));
        QTRY_COMPARE(controller.switchingTo(), QString());
        QCOMPARE(controller.actionError(), QString());
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("SetMode:read-only:no-force")));

        m_daemon->account->set({{QStringLiteral("Mode"), QStringLiteral("read-write")}});
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-write"));
        controller.setMode(QStringLiteral("read-only"), true);
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-only"));
        QTRY_COMPARE(controller.switchingTo(), QString());
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("SetMode:read-only:force")));
    }

    /// Read-only asked for while changes are unsent is a state, not an error: the
    /// account stays read-write with the count shown, until it is forced, taken
    /// back, or the daemon switches by itself.
    void aSwitchToReadOnlyThatWaitsIsAState()
    {
        startFake();
        m_daemon->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}, {QStringLiteral("Mode"), QStringLiteral("read-write")}});
        m_daemon->account->unsentChanges = 3;
        AccountController controller(fake::FirstAccount);
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-write"));
        QVERIFY(!controller.switchingToReadOnly());

        controller.setMode(QStringLiteral("read-only"));
        QTRY_VERIFY(controller.switchingToReadOnly());
        QCOMPARE(controller.unsentCount(), 3ULL);
        QCOMPARE(controller.mode(), QStringLiteral("read-write"));
        QCOMPARE(controller.lastError(), QString());
        QCOMPARE(controller.actionError(), QString());
        QTRY_COMPARE(controller.switchingTo(), QString());

        // The count follows the daemon's.
        m_daemon->account->setReadOnlyWish(true, 1);
        QTRY_COMPARE(controller.unsentCount(), 1ULL);

        // Taken back: read-write, and nothing said.
        controller.setMode(QStringLiteral("read-write"));
        QTRY_VERIFY(!controller.switchingToReadOnly());
        QCOMPARE(controller.mode(), QStringLiteral("read-write"));
        QTRY_COMPARE(controller.switchingTo(), QString());
        QCOMPARE(controller.signInUrl(), QString());

        // Asked again and forced: switched at once.
        controller.setMode(QStringLiteral("read-only"));
        QTRY_VERIFY(controller.switchingToReadOnly());
        controller.setMode(QStringLiteral("read-only"), true);
        QTRY_COMPARE(controller.mode(), QStringLiteral("read-only"));
        QVERIFY(!controller.switchingToReadOnly());
        QCOMPARE(controller.unsentCount(), 0ULL);

        // LastError is another thing: an error is shown beside a wish, and takes nothing from it.
        m_daemon->account->set({{QStringLiteral("LastError"), QStringLiteral("no network")}});
        QTRY_COMPARE(controller.lastError(), QStringLiteral("no network"));
        QVERIFY(!controller.switchingToReadOnly());
        m_daemon->account->setReadOnlyWish(true, 4);
        QTRY_VERIFY(controller.switchingToReadOnly());
        QCOMPARE(controller.unsentCount(), 4ULL);
        QCOMPARE(controller.lastError(), QStringLiteral("no network"));
    }

    void signOutAndRefreshCallTheDaemon()
    {
        startFake();
        AccountController controller(fake::FirstAccount);
        QTRY_VERIFY(controller.serviceAvailable());
        controller.refreshAccountInfo();
        controller.signOut();
        QTRY_VERIFY(m_daemon->account->calls.contains(QStringLiteral("SignOut")));
        QVERIFY(m_daemon->account->calls.contains(QStringLiteral("RefreshInfo")));
    }
};

QTEST_GUILESS_MAIN(AccountControllerTest)

#include "accountcontrollertest.moc"
