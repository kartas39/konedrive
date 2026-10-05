#include "accountcontroller.h"
#include "accountsmodel.h"
#include "currentaccount.h"
#include "daemoncontroller.h"
#include "fakedaemon.h"
#include "synccontroller.h"

#include <KConfig>
#include <KConfigGroup>

#include <QAbstractItemModelTester>
#include <QFile>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QTest>

#include <memory>

namespace
{
const QString ClientId = QStringLiteral("0f8fad5b-d9cb-469f-a165-70867728950e");
}

/// The manager's side of the window: DaemonController (Accounts),
/// AccountsModel following Accounts, CurrentAccount and its remembered
/// choice, and Sign In. XDG_CONFIG_HOME is a temporary
/// directory, so konedriverc is never the user's.
class AccountsModelTest : public QObject
{
    Q_OBJECT

private:
    QTemporaryDir m_config;
    std::unique_ptr<FakeDaemon> m_daemon;

    void start(const QStringList &labels)
    {
        m_daemon = std::make_unique<FakeDaemon>(labels);
        QVERIFY(m_daemon->start());
    }

    static QStringList labels(const AccountsModel &model)
    {
        QStringList result;
        for (int row = 0; row < model.rowCount(); ++row) {
            result << model.data(model.index(row, 0), AccountsModel::LabelRole).toString();
        }
        return result;
    }

    QString remembered() const
    {
        KConfig config(m_config.filePath(QStringLiteral("konedriverc")), KConfig::SimpleConfig);
        return config.group(QStringLiteral("General")).readEntry("CurrentAccount", QString());
    }

private Q_SLOTS:
    void initTestCase()
    {
        QVERIFY(m_config.isValid());
        qputenv("XDG_CONFIG_HOME", QFile::encodeName(m_config.path()));
        QCOMPARE(QStandardPaths::writableLocation(QStandardPaths::GenericConfigLocation), m_config.path());
    }

    void init()
    {
        QFile::remove(m_config.filePath(QStringLiteral("konedriverc")));
    }

    void cleanup()
    {
        if (m_daemon) {
            m_daemon->stop();
        }
        m_daemon.reset();
    }

    /// One row per account, in the daemon's order, following accounts added
    /// and removed elsewhere (konedrivectl, say).
    void followsTheManager()
    {
        start({QStringLiteral("Personal"), QStringLiteral("Family")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::QtTest);
        QTRY_COMPARE(model.count(), 2);
        QTRY_COMPARE(labels(model), (QStringList{QStringLiteral("Personal"), QStringLiteral("Family")}));
        QCOMPARE(model.data(model.index(1, 0), AccountsModel::PathRole).toString(), fake::accountPath(fake::idFor(2)));
        QCOMPARE(model.data(model.index(1, 0), AccountsModel::IdRole).toString(), fake::idFor(2));

        m_daemon->addAccount(QStringLiteral("Work"));
        QTRY_COMPARE(labels(model), (QStringList{QStringLiteral("Personal"), QStringLiteral("Family"), QStringLiteral("Work")}));

        m_daemon->removeAccount(fake::FirstAccount);
        QTRY_COMPARE(labels(model), (QStringList{QStringLiteral("Family"), QStringLiteral("Work")}));

        // A row follows its account's own changes.
        m_daemon->object(0)->account->set({{QStringLiteral("Email"), QStringLiteral("family@example.com")}});
        QTRY_COMPARE(model.data(model.index(0, 0), AccountsModel::EmailRole).toString(), QStringLiteral("family@example.com"));
    }

    /// A daemon that goes away leaves the rows, each saying the service is gone.
    void theRowsOutliveTheDaemon()
    {
        start({QStringLiteral("Personal")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_COMPARE(model.count(), 1);
        QTRY_VERIFY(model.at(0)->sync()->serviceAvailable());
        m_daemon->stop();
        QTRY_VERIFY(!daemon.serviceAvailable());
        QTRY_VERIFY(!model.at(0)->sync()->serviceAvailable());
        QCOMPARE(model.count(), 1);
        QCOMPARE(model.at(0)->status()->text(), QStringLiteral("The KOneDrive service is not running"));
    }

    /// HelperState (dbus/org.konedrive.Accounts.xml) reaches the window and
    /// drives helperTrouble (what StatusPage's helper card and every
    /// account's status key off) and helperInstruction (what that card and
    /// the NoHelper prompt say to do about it).
    void helperStateDrivesTroubleAndItsInstruction()
    {
        start({QStringLiteral("Personal")});
        DaemonController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        QCOMPARE(controller.helperState(), QStringLiteral("connected"));
        QVERIFY(!controller.helperTrouble());
        QCOMPARE(controller.helperInstruction(), QString());

        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("not-installed")}});
        QTRY_VERIFY(controller.helperTrouble());
        QVERIFY2(controller.helperInstruction().contains(QStringLiteral("install-helper.sh")), qPrintable(controller.helperInstruction()));

        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("stopped")}});
        QTRY_VERIFY(controller.helperInstruction().contains(QStringLiteral("systemctl start")));

        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("failed")}});
        QTRY_VERIFY(controller.helperInstruction().contains(QStringLiteral("systemctl status")));

        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("unknown")}});
        QTRY_VERIFY(controller.helperTrouble());
        QVERIFY(!controller.helperInstruction().isEmpty());

        m_daemon->manager->set({{QStringLiteral("HelperState"), QStringLiteral("connected")}});
        QTRY_VERIFY(!controller.helperTrouble());
        QCOMPARE(controller.helperInstruction(), QString());
    }

    void theClientIdIsTheManagers()
    {
        start({});
        DaemonController controller;
        QTRY_VERIFY(controller.serviceAvailable());
        controller.setClientId(QStringLiteral("bad"));
        QTRY_VERIFY(controller.actionError().contains(QStringLiteral("invalid client ID")));

        controller.setClientId(QStringLiteral("  ") + ClientId + QStringLiteral(" "));
        QTRY_COMPARE(controller.clientId(), ClientId);
        QVERIFY(controller.actionError().isEmpty());
        QVERIFY(m_daemon->manager->calls.contains(QStringLiteral("SetClientId:") + ClientId));
    }

    /// The first account until one is chosen; the choice is remembered in
    /// konedriverc and comes back with the next start, even though the rows
    /// arrive one by one; an account removed gives way to the first.
    void theChoiceIsRemembered()
    {
        start({QStringLiteral("Personal"), QStringLiteral("Family")});
        const QString family = fake::accountPath(fake::idFor(2));
        {
            DaemonController daemon;
            AccountsModel model(&daemon);
            CurrentAccount current(&model);
            QCOMPARE(current.account(), nullptr);
            QTRY_COMPARE(current.path(), fake::FirstAccount);
            QCOMPARE(remembered(), QString());

            QSignalSpy changed(&current, &CurrentAccount::changed);
            current.select(family);
            QCOMPARE(current.path(), family);
            QCOMPARE(current.account()->label(), QStringLiteral("Family"));
            QCOMPARE(changed.count(), 1);
            QCOMPARE(remembered(), fake::idFor(2));
        }
        {
            DaemonController daemon;
            AccountsModel model(&daemon);
            CurrentAccount current(&model);
            QTRY_COMPARE(model.count(), 2);
            QCOMPARE(current.path(), family);

            m_daemon->removeAccount(family);
            QTRY_COMPARE(current.path(), fake::FirstAccount);
            m_daemon->removeAccount(fake::FirstAccount);
            QTRY_COMPARE(current.account(), nullptr);
            QCOMPARE(current.path(), QString());
        }
    }

    /// Trouble in an account not shown is never hidden: the switcher shows it.
    void anotherAccountsTroubleShows()
    {
        start({QStringLiteral("Personal"), QStringLiteral("Family")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        CurrentAccount current(&model);
        QTRY_COMPARE(current.path(), fake::FirstAccount);
        QVERIFY(!current.othersNeedAttention());

        auto *family = m_daemon->object(1);
        family->account->set({{QStringLiteral("State"), QStringLiteral("signed-in")}});
        family->sync->folder->set({{QStringLiteral("Path"), QStringLiteral("/home/u/Family")}, {QStringLiteral("State"), QStringLiteral("ready")}});
        family->sync->conflicts->set({{QStringLiteral("Count"), QVariant::fromValue<uint>(1)}});
        QTRY_VERIFY(current.othersNeedAttention());
        QCOMPARE(model.data(model.index(1, 0), AccountsModel::IconNameRole).toString(), QStringLiteral("state-warning"));

        // Shown, it is no longer "another".
        current.select(family->path);
        QVERIFY(!current.othersNeedAttention());
    }

    /// Sign In: the client id when none is set, then Accounts.SignIn, whose
    /// URL is opened in the browser; there is no account, so no row, until
    /// the daemon says the sign-in ended "signed-in", and then the account it
    /// names is chosen and shown.
    void addingSetsTheClientIdSignsInAndChooses()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        CurrentAccount current(&model);
        QTRY_VERIFY(daemon.serviceAvailable());

        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);
        model.addAccount(ClientId);
        QVERIFY(model.adding());
        QTRY_COMPARE(open.count(), 1);
        const QString path = fake::FirstAccount;
        QCOMPARE(open.at(0).at(0).toString(), QStringLiteral("https://login.example/authorize?sign_in=1"));
        QCOMPARE(m_daemon->manager->calls, (QStringList{QStringLiteral("SetClientId:") + ClientId, QStringLiteral("SignIn")}));
        // Nothing is made before the sign-in has succeeded.
        QCOMPARE(m_daemon->objects.size(), 0);
        QCOMPARE(model.count(), 0);
        QVERIFY(model.adding());

        // The sign-in completes in the browser.
        m_daemon->finishSignIn(QStringLiteral("signed-in"), QStringLiteral("ann@example.com"));
        QTRY_COMPARE(added.count(), 1);
        QCOMPARE(added.at(0).at(0).toString(), path);
        QVERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QCOMPARE(current.path(), path);
        QCOMPARE(remembered(), fake::idFor(1));

        QCOMPARE(model.count(), 1);
        QTRY_COMPARE(model.at(0)->account()->label(), QStringLiteral("ann@example.com"));
        QVERIFY(model.anySignedIn());

        // With a client id already set, Sign In goes straight to SignIn.
        m_daemon->manager->calls.clear();
        model.addAccount(QString());
        QTRY_COMPARE(open.count(), 2);
        m_daemon->finishSignIn(QStringLiteral("signed-in"), QStringLiteral("bea@example.com"));
        QTRY_COMPARE(added.count(), 2);
        QCOMPARE(m_daemon->manager->calls.first(), QStringLiteral("SignIn"));
        QTRY_COMPARE(current.account()->label(), QStringLiteral("bea@example.com"));
    }

    /// The outcomes that make no account: what the window says for each.
    void aSignInThatAddsNothingSaysWhy_data()
    {
        QTest::addColumn<QString>("outcome");
        QTest::addColumn<QString>("message");
        QTest::addColumn<QString>("error");
        QTest::newRow("already-added") << QStringLiteral("already-added") << QStringLiteral("Personal") << QStringLiteral("This account is already added as Personal.");
        QTest::newRow("failed") << QStringLiteral("failed") << QStringLiteral("the sign-in timed out") << QStringLiteral("the sign-in timed out");
        QTest::newRow("failed, no message") << QStringLiteral("failed") << QString() << QStringLiteral("The account could not be added.");
        // Cancelled from elsewhere: a newer sign-in, started with konedrivectl, replaced it.
        QTest::newRow("cancelled") << QStringLiteral("cancelled") << QString() << QString();
    }

    void aSignInThatAddsNothingSaysWhy()
    {
        QFETCH(QString, outcome);
        QFETCH(QString, message);
        QFETCH(QString, error);
        start({QStringLiteral("Personal")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_COMPARE(model.count(), 1);
        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);

        model.addAccount(QString());
        QTRY_COMPARE(open.count(), 1);
        m_daemon->finishSignIn(outcome, message);
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), error);
        QCOMPARE(added.count(), 0);
        QCOMPARE(model.count(), 1);
    }

    /// Cancel: Accounts.CancelSignIn with the sign-in's number, and no error.
    /// Asked before SignIn has answered, it is done as soon as the answer
    /// gives the number, and the browser is not opened.
    void cancellingASignIn()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());
        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);

        model.addAccount(QString());
        QTRY_COMPARE(open.count(), 1);
        model.cancelAdd();
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QVERIFY(m_daemon->manager->calls.contains(QStringLiteral("CancelSignIn:1")));
        QCOMPARE(m_daemon->signIn, 0u);

        // Before SignIn has answered.
        model.addAccount(QString());
        model.cancelAdd();
        QVERIFY(model.adding());
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QVERIFY(m_daemon->manager->calls.contains(QStringLiteral("CancelSignIn:2")));
        QCOMPARE(m_daemon->signIn, 0u);
        QCOMPARE(open.count(), 1);
        QCOMPARE(added.count(), 0);
        QCOMPARE(model.count(), 0);
    }

    /// A cancel that comes while the daemon is making the account cancels
    /// nothing: CancelSignIn answers false, and "signed-in" follows. The
    /// account is shown.
    void aCancelTooLateStillShowsTheAccount()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());
        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);

        m_daemon->manager->cancelEnds = false;
        model.addAccount(QString());
        QTRY_COMPARE(open.count(), 1);
        model.cancelAdd();
        QTRY_VERIFY(m_daemon->manager->calls.contains(QStringLiteral("CancelSignIn:1")));
        QVERIFY(model.adding());
        m_daemon->finishSignIn(QStringLiteral("signed-in"), QStringLiteral("ann@example.com"));
        QTRY_COMPARE(added.count(), 1);
        QCOMPARE(added.at(0).at(0).toString(), fake::FirstAccount);
        QVERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
    }

    /// SignInFinished sent before SignIn's own answer is not lost.
    void theOutcomeMayComeBeforeSignInAnswers()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());
        QSignalSpy added(&model, &AccountsModel::accountAdded);

        m_daemon->manager->finishBeforeReply = {QStringLiteral("failed"), QStringLiteral("no network")};
        model.addAccount(QString());
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QStringLiteral("no network"));
        QCOMPARE(model.count(), 0);

        m_daemon->manager->finishBeforeReply = {QStringLiteral("signed-in"), QStringLiteral("ann@example.com")};
        model.addAccount(QString());
        QTRY_COMPARE(added.count(), 1);
        QCOMPARE(added.at(0).at(0).toString(), fake::FirstAccount);
        QVERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QCOMPARE(model.count(), 1);
    }

    /// The outcome of another client's sign-in (konedrivectl's, say) is not
    /// this window's, whether it comes before SignIn has answered or after.
    void anotherSignInsOutcomeIsIgnored()
    {
        start({QStringLiteral("Personal")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_COMPARE(model.count(), 1);
        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);
        QSignalSpy finished(&daemon, &DaemonController::signInFinished);

        // Before the answer: this window's number is not known yet.
        model.addAccount(QString());
        m_daemon->sendFinished(7, QStringLiteral("failed"), QStringLiteral("not ours"), QStringLiteral("/"));
        QTRY_COMPARE(open.count(), 1);
        QCOMPARE(finished.count(), 1);
        QVERIFY(model.adding());

        // After it.
        m_daemon->sendFinished(8, QStringLiteral("signed-in"), QStringLiteral("Personal"), fake::FirstAccount);
        m_daemon->sendFinished(9, QStringLiteral("already-added"), QStringLiteral("Personal"), fake::FirstAccount);
        QTRY_COMPARE(finished.count(), 3);
        QVERIFY(model.adding());
        QCOMPARE(model.addError(), QString());
        QCOMPARE(added.count(), 0);

        // Its own still ends it.
        m_daemon->finishSignIn(QStringLiteral("failed"), QStringLiteral("the sign-in timed out"));
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QStringLiteral("the sign-in timed out"));
        QCOMPARE(added.count(), 0);
    }

    void aRefusedAddSaysWhy()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());
        QSignalSpy added(&model, &AccountsModel::accountAdded);

        model.addAccount(QStringLiteral("bad"));
        QTRY_VERIFY(!model.adding());
        QVERIFY2(model.addError().contains(QStringLiteral("invalid client ID")), qPrintable(model.addError()));
        QVERIFY(!m_daemon->manager->calls.contains(QStringLiteral("SignIn")));
        QCOMPARE(added.count(), 0);

        // The dialog opens clean next time.
        model.clearAddError();
        QCOMPARE(model.addError(), QString());

        // SignIn itself refused.
        m_daemon->manager->refuseSignIn = QStringLiteral("config.toml cannot be read");
        model.addAccount(QString());
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QStringLiteral("config.toml cannot be read"));
        QCOMPARE(m_daemon->signIn, 0u);
    }

    /// Cancel pressed before the answer, and the answer is a refusal: there is
    /// nothing to cancel and nothing to say.
    void aCancelIsNotAnsweredWithARefusal()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());

        // SignIn refused.
        m_daemon->manager->refuseSignIn = QStringLiteral("config.toml cannot be read");
        model.addAccount(QString());
        model.cancelAdd();
        QVERIFY(model.adding());
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QVERIFY(m_daemon->manager->calls.contains(QStringLiteral("SignIn")));

        // SetClientId refused.
        m_daemon->manager->calls.clear();
        model.addAccount(QStringLiteral("bad"));
        model.cancelAdd();
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QVERIFY(!m_daemon->manager->calls.contains(QStringLiteral("SignIn")));
    }

    /// A daemon that goes away while an account is being added sends no
    /// SignInFinished: the adding ends, and says why. While SignIn has not
    /// answered either, the failed call that follows says nothing more.
    void theDaemonLeavingEndsTheAdding()
    {
        start({});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_VERIFY(daemon.serviceAvailable());
        QSignalSpy added(&model, &AccountsModel::accountAdded);
        QSignalSpy open(&model, &AccountsModel::openUrlRequested);
        const QString stopped = QStringLiteral("The KOneDrive service stopped before the account was added. Sign in again.");

        // With the browser open.
        model.addAccount(QString());
        QTRY_COMPARE(open.count(), 1);
        m_daemon->stop();
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), stopped);
        QCOMPARE(added.count(), 0);

        // The daemon is back with no sign-in under way, as it starts: the next Sign In works.
        QVERIFY(m_daemon->start());
        QTRY_VERIFY(daemon.serviceAvailable());
        model.addAccount(QString());
        QVERIFY(model.addError().isEmpty());
        QTRY_COMPARE(open.count(), 2);

        // Cancel pressed, and the daemon goes before it says "cancelled": no error.
        m_daemon->manager->cancelEnds = false;
        model.cancelAdd();
        QTRY_VERIFY(m_daemon->manager->calls.contains(QStringLiteral("CancelSignIn:2")));
        QVERIFY(model.adding());
        m_daemon->stop();
        QTRY_VERIFY(!model.adding());
        QCOMPARE(model.addError(), QString());
        QCOMPARE(added.count(), 0);
    }

    /// The rules of a label, checked before the daemon is asked.
    void labelProblems()
    {
        start({QStringLiteral("Personal")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_COMPARE(model.count(), 1);
        QTRY_COMPARE(model.at(0)->account()->label(), QStringLiteral("Personal"));

        QCOMPARE(model.labelProblem(QStringLiteral(" Family ")), QString());
        QCOMPARE(model.labelProblem(QString(40, QChar(0x0416))), QString()); // 40 Cyrillic letters are 40 characters
        QVERIFY(!model.labelProblem(QStringLiteral("   ")).isEmpty());
        QVERIFY(!model.labelProblem(QString(41, QLatin1Char('a'))).isEmpty());
        QVERIFY(!model.labelProblem(QStringLiteral("a/b")).isEmpty());
        // "@" is allowed: an account's name is commonly its email (A14).
        QCOMPARE(model.labelProblem(QStringLiteral("ann@home")), QString());
        QVERIFY(!model.labelProblem(QStringLiteral("a\tb")).isEmpty());
        // What an account id looks like, in any case, is not a name…
        QVERIFY(model.labelProblem(QStringLiteral("3f9a1c0e5b7d")).contains(QStringLiteral("hexadecimal")));
        QVERIFY(model.labelProblem(QStringLiteral(" 3F9A1C0E5B7D ")).contains(QStringLiteral("hexadecimal")));
        // …but a shorter, longer or not-quite-hex one is.
        QCOMPARE(model.labelProblem(QStringLiteral("3f9a1c0e5b7")), QString());
        QCOMPARE(model.labelProblem(QStringLiteral("3f9a1c0e5b7d0")), QString());
        QCOMPARE(model.labelProblem(QStringLiteral("3f9a1c0e5b7g")), QString());
        QVERIFY(model.labelProblem(QStringLiteral("PERSONAL")).contains(QStringLiteral("Personal")));
        // Renaming an account to its own name in another case is not a clash.
        QCOMPARE(model.labelProblem(QStringLiteral("PERSONAL"), fake::FirstAccount), QString());
    }

    void removingAnAccount()
    {
        start({QStringLiteral("Personal"), QStringLiteral("Family")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        QTRY_COMPARE(model.count(), 2);
        QSignalSpy removed(&model, &AccountsModel::accountRemoved);

        // Refused (an intercepted folder with no helper): nothing changes,
        // and it says why, for that account only, and that the helper is the way.
        m_daemon->manager->refuseRemove = true;
        model.removeAccount(fake::FirstAccount);
        QCOMPARE(daemon.removing(), fake::FirstAccount);
        // One at a time: a second Remove while one is under way is not sent.
        model.removeAccount(fake::FirstAccount);
        QTRY_COMPARE(daemon.removing(), QString());
        QCOMPARE(m_daemon->manager->calls.count(QStringLiteral("Remove:") + fake::FirstAccount), 1);
        QCOMPARE(daemon.removeFailedPath(), fake::FirstAccount);
        QVERIFY(daemon.removeError().contains(QStringLiteral("helper")));
        QVERIFY(daemon.removeNeedsHelper());
        QCOMPARE(daemon.actionError(), QString());
        QCOMPARE(model.count(), 2);

        m_daemon->manager->refuseRemove = false;
        model.removeAccount(fake::FirstAccount);
        QTRY_COMPARE(model.count(), 1);
        QCOMPARE(removed.count(), 1);
        QCOMPARE(daemon.removeFailedPath(), QString());
        QVERIFY(!daemon.removeNeedsHelper());
        QCOMPARE(labels(model), QStringList{QStringLiteral("Family")});
    }

    /// A daemon that comes back with other accounts (removed and added with
    /// konedrivectl while it was away): the rows follow, and the window falls
    /// back to the first account when the one it showed is gone.
    void aDaemonBackWithOtherAccounts()
    {
        start({QStringLiteral("Personal"), QStringLiteral("Family")});
        DaemonController daemon;
        AccountsModel model(&daemon);
        CurrentAccount current(&model);
        QTRY_COMPARE(model.count(), 2);
        const QString family = fake::accountPath(fake::idFor(2));
        current.select(family);

        m_daemon->stop();
        QTRY_VERIFY(!daemon.serviceAvailable());
        m_daemon->removeAccount(family);
        m_daemon->addAccount(QStringLiteral("Work"));
        QCOMPARE(model.count(), 2);
        QCOMPARE(current.path(), family);

        QVERIFY(m_daemon->start());
        QTRY_COMPARE(labels(model), (QStringList{QStringLiteral("Personal"), QStringLiteral("Work")}));
        QCOMPARE(current.path(), fake::FirstAccount);
        QTRY_VERIFY(model.at(1)->sync()->serviceAvailable());
    }
};

QTEST_GUILESS_MAIN(AccountsModelTest)

#include "accountsmodeltest.moc"
