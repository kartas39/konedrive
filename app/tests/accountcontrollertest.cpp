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
